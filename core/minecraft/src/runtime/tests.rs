use super::file_download::component_manifest_link_target_path;
use super::{
    ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
    ComponentManifestFile, JavaRuntimeInfo, JavaRuntimeLookupError, MachOArm64Compatibility,
    ManagedRuntimeCache, ManagedRuntimeRebuildError, RosettaRuntimeDecision, RuntimeDownloadActual,
    RuntimeDownloadEvidence, RuntimeDownloadIntegrityError, RuntimeDownloadManifest,
    RuntimeEnsureEvent, RuntimeId, RuntimeInstallState, RuntimeManifest, RuntimeRecord,
    RuntimeSource, RuntimeSourceFailure, RuntimeSourceFailureKind, RuntimeSourceReceipt,
    acquire_runtime_source_for_test, active_runtime_file_lock_workers_for_test,
    authenticated_runtime_source_from_manifest_for_test, block_runtime_decompression_for_test,
    component_manifest_destination, detect_distribution, detect_runtime_state,
    discard_staged_managed_runtime, ensure_runtime_with_events, fetch_runtime_file,
    fetch_runtime_manifest_bytes_for_test, finalize_managed_runtime_commit_with_failure_for_test,
    finalize_managed_runtime_commit_with_removed_quarantine_failure_for_test,
    install_runtime_manifest_file, install_runtime_manifest_files, java_executable,
    java_executable_for_os, managed_runtime_contents_verified_without_probe,
    materialize_preferred_runtime_source, parse_mach_o_arm64_compatibility,
    plan_runtime_manifest_files, publish_staged_managed_runtime,
    publish_staged_managed_runtime_and_finalize,
    publish_staged_managed_runtime_with_displacement_failure_for_test,
    publish_staged_managed_runtime_with_finalization_failure_for_test,
    publish_staged_managed_runtime_with_promotion_failure_for_test,
    publish_staged_managed_runtime_with_restoration_failure_for_test,
    publish_staged_managed_runtime_with_rotation_failure_for_test,
    rebuild_managed_runtime_component_from_source,
    register_runtime_tree_verification_counts_for_test, rosetta_requirement_for_managed_runtime,
    runtime_cancellation_channel, runtime_download_client, runtime_file_download_concurrency_for,
    runtime_install_lock_file_path, runtime_materialization_control, runtime_os_arch_for,
    runtime_publication_lock_availability_for_test, runtime_record_matches_source_for_test,
    runtime_source_url_is_secure_for_test, runtime_windows_verbatim_path_string,
    select_runtime_manifest, stage_managed_runtime, stage_managed_runtime_until_cancelled,
    take_runtime_tree_verification_counts_for_test, validate_ephemeral_processor_manifest_for_test,
    validate_runtime_file_source_urls_for_test, verify_runtime_download,
};
#[cfg(feature = "test-support")]
use super::{
    ManagedRuntimeMutationRefused, component_manifest_proof_bytes,
    ensure_runtime_with_persisted_manifest_for_test,
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
use tokio::sync::oneshot;

#[cfg(feature = "test-support")]
struct RuntimeMutationPermitProbe {
    active: Arc<AtomicUsize>,
}

#[cfg(feature = "test-support")]
impl Drop for RuntimeMutationPermitProbe {
    fn drop(&mut self) {
        assert_eq!(self.active.swap(0, Ordering::SeqCst), 1);
    }
}

fn expected(size: Option<u64>, sha1: Option<&str>) -> RuntimeDownloadEvidence {
    RuntimeDownloadEvidence {
        size,
        sha1: sha1.map(str::to_string),
    }
}

fn test_runtime_component() -> RuntimeId {
    RuntimeId::from("java-runtime-delta")
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
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::PolicyRejected =>
        {
            failure.detail().to_string()
        }
        other => panic!("expected unsafe manifest path error, got {other:?}"),
    }
}

fn expect_runtime_source_failure(
    error: JavaRuntimeLookupError,
    expected_kind: RuntimeSourceFailureKind,
) -> RuntimeSourceFailure {
    match error {
        JavaRuntimeLookupError::RuntimeSource(failure)
            if failure.component() == &test_runtime_component()
                && failure.kind() == expected_kind =>
        {
            failure
        }
        other => panic!("expected {expected_kind:?} runtime source failure, got {other:?}"),
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
    let destination =
        component_manifest_destination(&test_runtime_component(), temp_dir, "bin/java").unwrap();

    assert_eq!(destination, temp_dir.join("bin").join("java"));
}

#[test]
fn component_manifest_destination_rejects_traversal() {
    let temp_dir = Path::new("runtime-temp");
    let message = unsafe_manifest_path_message(component_manifest_destination(
        &test_runtime_component(),
        temp_dir,
        "bin/../java",
    ));

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
    let message = unsafe_manifest_path_message(component_manifest_destination(
        &test_runtime_component(),
        temp_dir,
        absolute_path,
    ));

    assert!(message.contains("unsafe runtime manifest path"));
    assert!(message.contains(absolute_path));
    assert!(!message.contains("runtime-temp"));
}

#[test]
fn component_manifest_destination_rejects_drive_like_path_with_slashes() {
    let temp_dir = Path::new("runtime-temp");
    let message = unsafe_manifest_path_message(component_manifest_destination(
        &test_runtime_component(),
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
        &test_runtime_component(),
        temp_dir,
        r"C:\Windows\System32",
    ));

    assert!(message.contains("unsafe runtime manifest path"));
    assert!(message.contains(r"C:\Windows\System32"));
    assert!(!message.contains("runtime-temp"));
}

#[test]
fn component_manifest_destination_rejects_nonportable_segments() {
    let overlong = "a".repeat(crate::artifact_path::MAX_ARTIFACT_PATH_SEGMENT_BYTES + 1);
    for relative_path in [
        "bin/NUL.txt",
        "bin/java.",
        "bin/java ",
        "bin/ja*va",
        "bin/ja\0va",
        overlong.as_str(),
    ] {
        let message = unsafe_manifest_path_message(component_manifest_destination(
            &test_runtime_component(),
            Path::new("runtime-temp"),
            relative_path,
        ));

        assert!(
            message.contains("unsafe runtime manifest path"),
            "unexpected rejection for {relative_path:?}: {message}"
        );
    }
}

#[test]
fn component_manifest_link_target_rejects_nonportable_named_segments() {
    for target in ["../NUL.txt", "../license.", "../license ", "../li*ense"] {
        let result = component_manifest_link_target_path(
            &test_runtime_component(),
            Path::new("runtime"),
            Path::new("runtime/legal/module/LICENSE"),
            "legal/module/LICENSE",
            target,
        );

        assert!(matches!(
            result,
            Err(JavaRuntimeLookupError::RuntimeSource(failure))
                if failure.component() == &test_runtime_component()
                    && failure.kind() == RuntimeSourceFailureKind::PolicyRejected
                    && failure.detail().contains("unsafe runtime manifest link target")
        ));
    }
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

    install_runtime_manifest_files(&test_runtime_component(), &root, files, &mut |event| {
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

    install_runtime_manifest_files(&test_runtime_component(), &root, files, &mut |event| {
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
        &test_runtime_component(),
        runtime_download_client().clone(),
        &root,
        "bin/java-link",
        manifest_link("../../outside"),
    )
    .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::PolicyRejected
                && failure.detail().contains("unsafe runtime manifest link target")
    ));
    assert!(!root.join("bin").join("java-link").exists());
    let _ = fs::remove_dir_all(root);
}

#[cfg(not(unix))]
#[tokio::test]
async fn runtime_manifest_link_fails_explicitly_on_non_unix() {
    let root = unique_temp_root("axial-runtime-link-non-unix-test");
    let result = install_runtime_manifest_file(
        &test_runtime_component(),
        runtime_download_client().clone(),
        &root,
        "bin/java-link",
        manifest_link("java"),
    )
    .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::Install(message))
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

    let failure = expect_runtime_source_failure(error, RuntimeSourceFailureKind::Unavailable);
    assert!(failure.detail().contains("HTTP 503"), "{failure:?}");
}

#[tokio::test]
async fn runtime_manifest_json_fetch_classifies_permanent_http_status_as_metadata_invalid() {
    let url = serve_runtime_json(404, b"missing".to_vec(), None).await;

    let error = fetch_runtime_manifest_bytes_for_test(&url)
        .await
        .expect_err("permanent HTTP error should fail");

    let failure = expect_runtime_source_failure(error, RuntimeSourceFailureKind::MetadataInvalid);
    assert!(failure.detail().contains("HTTP 404"), "{failure:?}");
}

#[tokio::test]
async fn runtime_manifest_json_fetch_classifies_transport_failure_as_unavailable() {
    let error = fetch_runtime_manifest_bytes_for_test("http://127.0.0.1:9/runtime.json")
        .await
        .expect_err("connection failure should fail");

    expect_runtime_source_failure(error, RuntimeSourceFailureKind::Unavailable);
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

    let failure = expect_runtime_source_failure(error, RuntimeSourceFailureKind::PolicyRejected);
    assert_eq!(failure.detail(), "runtime manifest response too large");
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

#[test]
fn runtime_file_source_policy_classifies_invalid_and_insecure_urls() {
    for (url, expected_kind) in [
        ("not a URL", RuntimeSourceFailureKind::MetadataInvalid),
        (
            "http://provider.invalid/runtime.bin",
            RuntimeSourceFailureKind::PolicyRejected,
        ),
        (
            "file:///tmp/runtime.bin",
            RuntimeSourceFailureKind::PolicyRejected,
        ),
    ] {
        let manifest = ComponentManifest {
            files: HashMap::from([(
                "bin/java".to_string(),
                downloadable_manifest_file(url, 1, "0000000000000000000000000000000000000000"),
            )]),
        };

        let error =
            validate_runtime_file_source_urls_for_test(&test_runtime_component(), &manifest)
                .expect_err("invalid runtime file source should fail");

        expect_runtime_source_failure(error, expected_kind);
    }
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
async fn ready_managed_runtime_matches_the_full_authenticated_source() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    let source = runtime_source_receipt_fixture(&component, &root, b"authenticated java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");
    let verified = publish_staged_managed_runtime(staged)
        .await
        .expect("managed runtime publish")
        .into_verified_runtime(&cache, &component, 8)
        .expect("verified managed runtime");
    let (_resolved, source) = verified.into_parts();
    let runtime = RuntimeRecord {
        id: component,
        java_path: java_executable(&root).to_string_lossy().into_owned(),
        info: JavaRuntimeInfo {
            id: "jre-legacy".to_string(),
            major: 8,
            update: 0,
            distribution: "test".to_string(),
            path: java_executable(&root).to_string_lossy().into_owned(),
        },
        source: RuntimeSource::Managed,
        install_state: RuntimeInstallState::Ready,
        root_dir: root.to_string_lossy().into_owned(),
    };

    assert!(runtime_record_matches_source_for_test(&runtime, &source).await);
    fs::write(java_executable(&root), b"tampered java").expect("tamper runtime file");
    make_executable(&java_executable(&root));
    assert!(!runtime_record_matches_source_for_test(&runtime, &source).await);
}

#[tokio::test]
async fn managed_runtime_materialization_scan_budgets_are_one_cached_and_three_fresh() {
    let cached = ManagedRuntimeCache::isolated_for_test().expect("cached runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let cached_root = cached
        .component_root(component.as_str())
        .expect("cached runtime root");
    let cached_source =
        runtime_source_receipt_fixture(&component, &cached_root, b"cached java").await;
    let cached_stage = stage_managed_runtime(&cached, &component, cached_source, &mut |_| {})
        .await
        .expect("cached runtime stage");
    let cached_verified = publish_staged_managed_runtime_and_finalize(cached_stage)
        .await
        .expect("cached runtime publish")
        .into_verified_runtime(&cached, &component, 8)
        .expect("cached verified runtime");
    let (_runtime, cached_source) = cached_verified.into_parts();
    register_runtime_tree_verification_counts_for_test(&cached_root);

    let (_cached_cancel, mut cached_control) = runtime_materialization_control();
    let cached_result = materialize_preferred_runtime_source(
        &cached,
        &JavaVersion {
            component: component.as_str().to_string(),
            major_version: 8,
        },
        cached_source,
        &mut |_| {},
        &mut cached_control,
    )
    .await
    .expect("cached runtime materialization");
    assert!(cached_result.is_some());
    let cached_counts = take_runtime_tree_verification_counts_for_test(&cached_root);
    assert_eq!(cached_counts.reason_vector(), [0, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(cached_counts.total(), 1);

    let fresh = ManagedRuntimeCache::isolated_for_test().expect("fresh runtime cache");
    let fresh_root = fresh
        .component_root(component.as_str())
        .expect("fresh runtime root");
    let fresh_source = runtime_source_receipt_fixture(&component, &fresh_root, b"fresh java").await;
    register_runtime_tree_verification_counts_for_test(&fresh_root);
    let (_fresh_cancel, mut fresh_control) = runtime_materialization_control();
    let fresh_result = materialize_preferred_runtime_source(
        &fresh,
        &JavaVersion {
            component: component.as_str().to_string(),
            major_version: 8,
        },
        fresh_source,
        &mut |_| {},
        &mut fresh_control,
    )
    .await
    .expect("fresh runtime materialization");
    assert!(fresh_result.is_some());
    let fresh_counts = take_runtime_tree_verification_counts_for_test(&fresh_root);
    assert_eq!(fresh_counts.reason_vector(), [1, 1, 0, 1, 0, 0, 0, 0]);
    assert_eq!(fresh_counts.total(), 3);
}

#[tokio::test]
async fn managed_runtime_publication_scan_budgets_cover_reuse_and_replacement() {
    let component = RuntimeId::from("jre-legacy");

    let reuse_cache = ManagedRuntimeCache::isolated_for_test().expect("reuse runtime cache");
    let reuse_root = reuse_cache
        .component_root(component.as_str())
        .expect("reuse runtime root");
    let reuse_source = runtime_source_receipt_fixture_with_download_count(
        &component,
        &reuse_root,
        b"reused java",
        2,
    )
    .await;
    let reuse_stage = stage_managed_runtime(&reuse_cache, &component, reuse_source, &mut |_| {})
        .await
        .expect("initial reuse stage");
    let reuse_source = publish_staged_managed_runtime_and_finalize(reuse_stage)
        .await
        .expect("initial reuse publish")
        .into_verified_runtime(&reuse_cache, &component, 8)
        .expect("initial verified reuse runtime")
        .into_parts()
        .1;
    register_runtime_tree_verification_counts_for_test(&reuse_root);
    let reuse_stage = stage_managed_runtime(&reuse_cache, &component, reuse_source, &mut |_| {})
        .await
        .expect("exact reuse stage");
    let reuse_receipt = publish_staged_managed_runtime_and_finalize(reuse_stage)
        .await
        .expect("exact canonical reuse");
    let reuse_counts = take_runtime_tree_verification_counts_for_test(&reuse_root);
    assert_eq!(reuse_counts.reason_vector(), [1, 1, 1, 0, 0, 0, 0, 0]);
    assert_eq!(reuse_counts.total(), 3);
    drop(reuse_receipt);

    let replacement_cache =
        ManagedRuntimeCache::isolated_for_test().expect("replacement runtime cache");
    let replacement_root = replacement_cache
        .component_root(component.as_str())
        .expect("replacement runtime root");
    let original_source =
        runtime_source_receipt_fixture(&component, &replacement_root, b"original replacement java")
            .await;
    let original_stage =
        stage_managed_runtime(&replacement_cache, &component, original_source, &mut |_| {})
            .await
            .expect("original replacement stage");
    let original_receipt = publish_staged_managed_runtime_and_finalize(original_stage)
        .await
        .expect("original replacement publish");
    drop(original_receipt);
    let replacement_source =
        runtime_source_receipt_fixture(&component, &replacement_root, b"new replacement java")
            .await;
    register_runtime_tree_verification_counts_for_test(&replacement_root);
    let replacement_stage = stage_managed_runtime(
        &replacement_cache,
        &component,
        replacement_source,
        &mut |_| {},
    )
    .await
    .expect("mismatching replacement stage");
    let replacement_receipt = publish_staged_managed_runtime_and_finalize(replacement_stage)
        .await
        .expect("mismatching canonical replacement");
    let replacement_counts = take_runtime_tree_verification_counts_for_test(&replacement_root);
    assert_eq!(replacement_counts.reason_vector(), [1, 1, 1, 1, 0, 0, 0, 0]);
    assert_eq!(replacement_counts.total(), 4);
    drop(replacement_receipt);
}

#[tokio::test]
async fn guardian_runtime_rebuild_scan_budget_is_five_with_two_simulated_state_boundaries() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("runtime root");
    let source = runtime_source_receipt_fixture(&component, &root, b"guardian java").await;
    register_runtime_tree_verification_counts_for_test(&root);

    let receipt =
        rebuild_managed_runtime_component_from_source(&cache, &component, source, &mut |_| {})
            .await
            .expect("Guardian runtime rebuild");
    assert!(receipt.revalidate(&cache, &component).await);
    assert!(receipt.revalidate(&cache, &component).await);

    let counts = take_runtime_tree_verification_counts_for_test(&root);
    assert_eq!(counts.reason_vector(), [1, 1, 0, 1, 0, 0, 2, 0]);
    assert_eq!(counts.total(), 5);
}

#[tokio::test]
async fn cached_runtime_verification_holds_exact_publication_locks_until_consumed() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("runtime root");
    let source = runtime_source_receipt_fixture(&component, &root, b"cached java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");
    let source = publish_staged_managed_runtime_and_finalize(staged)
        .await
        .expect("managed runtime publish")
        .into_verified_runtime(&cache, &component, 8)
        .expect("verified managed runtime")
        .into_parts()
        .1;
    let (_cancellation_sender, mut cancellation) = runtime_cancellation_channel();
    let verified = match super::install::verify_cached_managed_runtime_until_cancelled(
        &cache,
        &component,
        8,
        source,
        &mut cancellation,
    )
    .await
    .expect("cached runtime verification")
    {
        super::install::CachedManagedRuntimeVerification::Matched(verified) => verified,
        _ => panic!("cached runtime should match its authenticated source"),
    };
    assert_eq!(
        runtime_publication_lock_availability_for_test(&cache, &component),
        (false, false),
        "cached verification result must retain both exact publication locks"
    );
    let (_runtime, _source) = verified.into_parts();
    assert_eq!(
        runtime_publication_lock_availability_for_test(&cache, &component),
        (true, true),
        "verified result consumption must release both exact publication locks"
    );
}

#[tokio::test]
async fn cancelling_blocked_runtime_staging_removes_owned_sidecar() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    let staging_root = root.with_file_name("jre-legacy.staging");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("blocked runtime listener");
    let address = listener.local_addr().expect("blocked runtime address");
    let (request_started_tx, request_started_rx) = oneshot::channel();
    let (connection_closed_tx, connection_closed_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("runtime connection");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
        let _ = request_started_tx.send(());
        let mut remaining = Vec::new();
        let _ = socket.read_to_end(&mut remaining).await;
        let _ = connection_closed_tx.send(());
    });
    let java_bytes = b"blocked runtime";
    let java_relative_path = java_executable(&root)
        .strip_prefix(&root)
        .expect("java path under runtime root")
        .to_string_lossy()
        .replace('\\', "/");
    let mut java_file = downloadable_manifest_file(
        &format!("http://{address}/runtime.bin"),
        java_bytes.len() as u64,
        &sha1_hex(java_bytes),
    );
    java_file.executable = true;
    let source = authenticated_runtime_source_from_manifest_for_test(
        component.clone(),
        ComponentManifest {
            files: HashMap::from([(java_relative_path, java_file)]),
        },
    )
    .expect("authenticated blocked runtime source");
    let (cancellation_tx, mut cancellation) = runtime_cancellation_channel();
    let stage_cache = cache.clone();
    let stage_component = component.clone();
    let stage_task = tokio::spawn(async move {
        stage_managed_runtime_until_cancelled(
            &stage_cache,
            &stage_component,
            source,
            &mut |_| {},
            &mut cancellation,
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), request_started_rx)
        .await
        .expect("runtime download should begin")
        .expect("runtime request signal");
    assert!(staging_root.is_dir());
    cancellation_tx.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), stage_task)
        .await
        .expect("cancelled runtime stage should drain")
        .expect("runtime stage task")
        .expect("runtime stage cancellation should clean up");

    assert!(result.is_none());
    assert!(!staging_root.exists());
    assert!(!root.exists());
    tokio::time::timeout(std::time::Duration::from_secs(1), connection_closed_rx)
        .await
        .expect("cancelled runtime request should close")
        .expect("runtime connection close signal");
}

#[tokio::test]
async fn cancelling_external_file_lock_wait_leaves_no_worker_or_ghost_waiter() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    let lock_path = runtime_install_lock_file_path(&root);
    fs::create_dir_all(lock_path.parent().expect("runtime lock parent"))
        .expect("runtime lock parent");
    let external_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("external runtime lock");
    external_lock.lock().expect("hold runtime install lock");
    let java_bytes = b"locked runtime";
    let java_relative_path = java_executable(&root)
        .strip_prefix(&root)
        .expect("java path under runtime root")
        .to_string_lossy()
        .replace('\\', "/");
    let mut java_file = downloadable_manifest_file(
        "https://example.invalid/runtime.bin",
        java_bytes.len() as u64,
        &sha1_hex(java_bytes),
    );
    java_file.executable = true;
    let source = authenticated_runtime_source_from_manifest_for_test(
        component.clone(),
        ComponentManifest {
            files: HashMap::from([(java_relative_path, java_file)]),
        },
    )
    .expect("authenticated locked runtime source");
    let (cancellation_tx, mut cancellation) = runtime_cancellation_channel();
    let stage_cache = cache.clone();
    let stage_component = component.clone();
    let stage_task = tokio::spawn(async move {
        stage_managed_runtime_until_cancelled(
            &stage_cache,
            &stage_component,
            source,
            &mut |_| {},
            &mut cancellation,
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while active_runtime_file_lock_workers_for_test(&lock_path) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime file-lock worker should begin polling");
    cancellation_tx.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), stage_task)
        .await
        .expect("cancelled file-lock wait should drain")
        .expect("runtime stage task")
        .expect("runtime file-lock cancellation");

    assert!(result.is_none());
    assert_eq!(active_runtime_file_lock_workers_for_test(&lock_path), 0);
    assert!(!root.with_file_name("jre-legacy.staging").exists());
    external_lock
        .unlock()
        .expect("release external runtime lock");
    let contender = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("runtime lock contender");
    contender
        .try_lock()
        .expect("cancelled waiter must not acquire the lock later");
    contender.unlock().expect("release runtime lock contender");
}

#[tokio::test]
async fn cancelling_blocked_decompression_drains_worker_before_stage_cleanup() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    let staging_root = root.with_file_name("jre-legacy.staging");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"canonical").expect("canonical sentinel");
    let raw_bytes = b"replacement java".to_vec();
    let compressed_bytes = lzma_compress_bytes(&raw_bytes);
    let compressed_url = serve_runtime_download(compressed_bytes.clone()).await;
    let java_relative_path = java_executable(&root)
        .strip_prefix(&root)
        .expect("java path under runtime root")
        .to_string_lossy()
        .replace('\\', "/");
    let mut java_file = downloadable_lzma_manifest_file(
        &compressed_url,
        raw_bytes.len() as u64,
        &sha1_hex(&raw_bytes),
        &compressed_url,
        compressed_bytes.len() as u64,
        &sha1_hex(&compressed_bytes),
    );
    java_file.executable = true;
    let source = authenticated_runtime_source_from_manifest_for_test(
        component.clone(),
        ComponentManifest {
            files: HashMap::from([(java_relative_path, java_file)]),
        },
    )
    .expect("authenticated compressed runtime source");
    let contender_source =
        runtime_source_receipt_fixture(&component, &root, b"contender java").await;
    let mut decompression_temp = java_executable(&staging_root);
    let mut decompression_name = decompression_temp
        .file_name()
        .expect("staged java file name")
        .to_os_string();
    decompression_name.push(".axial-tmp");
    decompression_temp.set_file_name(decompression_name);
    let gate = block_runtime_decompression_for_test(decompression_temp);
    let (cancellation_tx, mut cancellation) = runtime_cancellation_channel();
    let stage_cache = cache.clone();
    let stage_component = component.clone();
    let mut stage_task = tokio::spawn(async move {
        stage_managed_runtime_until_cancelled(
            &stage_cache,
            &stage_component,
            source,
            &mut |_| {},
            &mut cancellation,
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match gate.started.try_recv() {
                Ok(()) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("decompression test worker stopped before blocking")
                }
            }
        }
    })
    .await
    .expect("runtime decompression worker should block");
    cancellation_tx.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut stage_task)
            .await
            .is_err(),
        "stage must retain its worker, sidecar, and locks while decompression is blocked"
    );
    assert!(staging_root.exists());
    assert_eq!(
        runtime_publication_lock_availability_for_test(&cache, &component),
        (false, false),
        "blocked decompression must retain both publication locks"
    );
    assert_eq!(
        fs::read(root.join("sentinel")).expect("canonical sentinel while cancelled"),
        b"canonical"
    );

    gate.release
        .send(())
        .expect("release runtime decompression worker");
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), stage_task)
        .await
        .expect("cancelled decompression should drain")
        .expect("runtime stage task")
        .expect("runtime decompression cancellation");
    assert!(result.is_none());
    assert!(!staging_root.exists());
    assert_eq!(
        fs::read(root.join("sentinel")).expect("canonical sentinel after cancellation"),
        b"canonical"
    );
    assert_eq!(
        runtime_publication_lock_availability_for_test(&cache, &component),
        (true, true),
        "cancelled stage must clean up before releasing publication locks"
    );

    let contender_cache = cache.clone();
    let contender_component = component.clone();
    let contender_task = tokio::spawn(async move {
        stage_managed_runtime(
            &contender_cache,
            &contender_component,
            contender_source,
            &mut |_| {},
        )
        .await
    });
    let contender = tokio::time::timeout(std::time::Duration::from_secs(2), contender_task)
        .await
        .expect("runtime stage contender should acquire released ownership")
        .expect("runtime stage contender task")
        .expect("runtime stage contender");
    discard_staged_managed_runtime(contender)
        .await
        .expect("discard runtime stage contender");
    assert!(!staging_root.exists());
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

    let failure = expect_runtime_source_failure(error, RuntimeSourceFailureKind::IntegrityMismatch);
    assert_eq!(failure.detail(), "runtime component manifest size mismatch");
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

    let failure = expect_runtime_source_failure(error, RuntimeSourceFailureKind::IntegrityMismatch);
    assert_eq!(
        failure.detail(),
        "runtime component manifest checksum mismatch"
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

    let failure = expect_runtime_source_failure(error, RuntimeSourceFailureKind::MetadataInvalid);
    assert!(failure.detail().contains("expected ident"), "{failure:?}");
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
    let temp = unique_temp_root("axial-managed-runtime-manifest-drift-test");
    let root = temp.join("java-runtime-delta");
    write_runtime_executable_fixture(&root);
    write_runtime_manifest_proof_for_java(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(managed_runtime_contents_verified_without_probe(&root));

    fs::write(java_executable(&root), b"changed java").expect("modify java");
    make_executable(&java_executable(&root));

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(!managed_runtime_contents_verified_without_probe(&root));

    let _ = fs::remove_dir_all(temp);
}

#[cfg(unix)]
#[test]
fn explicit_full_runtime_verifier_detects_manifest_link_drift() {
    let temp = unique_temp_root("axial-managed-runtime-link-proof-test");
    let root = temp.join("java-runtime-delta");
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

    let _ = fs::remove_dir_all(temp);
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
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
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

    let staged =
        stage_managed_runtime(&cache, &component, receipt, &mut |event| events.push(event))
            .await
            .expect("fallback runtime stage");
    assert!(!root.exists());
    publish_staged_managed_runtime(staged)
        .await
        .expect("fallback runtime publish");

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(root.join(".axial-runtime-manifest.json").is_file());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEnsureEvent::ManagedRuntimeReady { .. }))
    );
}

#[tokio::test]
async fn managed_runtime_promotion_failure_restores_canonical_tree() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
    let source = runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
    let inventory = crate::known_good::runtime_inventory_from_source(&source)
        .expect("authenticated Runtime inventory");
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");
    let staging_root = staged.staging_root_for_test().to_path_buf();

    let error = publish_staged_managed_runtime_with_promotion_failure_for_test(staged)
        .await
        .expect_err("injected promotion failure");

    let ManagedRuntimeRebuildError::Effect(effect) = error else {
        panic!("canonical displacement must return sealed effect evidence");
    };
    assert_eq!(effect.component(), &component);
    assert!(effect.matches_cache(&cache));
    assert!(effect.matches_known_good_inventory(&inventory));
    assert!(effect.quarantine_obligation().is_none());
    assert_eq!(
        fs::read(root.join("sentinel")).expect("restored sentinel"),
        b"original"
    );
    assert!(!staging_root.exists());
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
}

#[tokio::test]
async fn atomic_displacement_failure_without_prior_effect_is_preparation() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
    let source = runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");
    let staging_root = staged.staging_root_for_test().to_path_buf();

    let error = publish_staged_managed_runtime_with_displacement_failure_for_test(staged)
        .await
        .expect_err("injected atomic displacement failure");

    assert!(matches!(error, ManagedRuntimeRebuildError::Preparation(_)));
    assert_eq!(
        fs::read(root.join("sentinel")).expect("unchanged canonical sentinel"),
        b"original"
    );
    assert!(!staging_root.exists());
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
}

#[tokio::test]
async fn managed_runtime_restoration_failure_retains_sealed_quarantine_evidence() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
    let source = runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");
    let staging_root = staged.staging_root_for_test().to_path_buf();

    let error = publish_staged_managed_runtime_with_restoration_failure_for_test(staged)
        .await
        .expect_err("injected restoration failure");

    let ManagedRuntimeRebuildError::Effect(effect) = error else {
        panic!("failed restoration must return sealed effect evidence");
    };
    let obligation = effect
        .quarantine_obligation()
        .expect("retained quarantine obligation");
    assert_eq!(obligation.component(), &component);
    assert!(obligation.matches_cache(&cache));
    assert_eq!(
        obligation.observation(),
        super::ManagedRuntimeQuarantineObservation::Present
    );
    assert!(!root.exists());
    assert!(!staging_root.exists());
    assert_eq!(
        fs::read(
            root.with_file_name("jre-legacy.quarantine")
                .join("sentinel")
        )
        .expect("quarantined canonical sentinel"),
        b"original"
    );
}

#[tokio::test]
async fn ordinary_managed_runtime_publish_finalizes_displaced_quarantine() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
    let source = runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");

    let receipt = publish_staged_managed_runtime_and_finalize(staged)
        .await
        .expect("ordinary managed runtime publish");

    assert!(receipt.quarantine_obligation().is_none());
    assert!(receipt.revalidate(&cache, &component).await);
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
}

#[tokio::test]
async fn ordinary_quarantine_finalization_failure_retains_effect_evidence() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
    let source = runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");

    let error = publish_staged_managed_runtime_with_finalization_failure_for_test(staged)
        .await
        .expect_err("injected quarantine finalization failure");

    let ManagedRuntimeRebuildError::Effect(effect) = error else {
        panic!("finalization failure must retain sealed effect truth");
    };
    let obligation = effect
        .quarantine_obligation()
        .expect("retained finalization obligation");
    assert!(obligation.matches_cache(&cache));
    assert_eq!(
        obligation.observation(),
        super::ManagedRuntimeQuarantineObservation::Present
    );
    assert!(managed_runtime_contents_verified_without_probe(&root));
    assert_eq!(
        fs::read(
            root.with_file_name("jre-legacy.quarantine")
                .join("sentinel")
        )
        .expect("retained displaced sentinel"),
        b"original"
    );
}

#[tokio::test]
async fn late_finalization_failure_reports_observed_quarantine_and_retains_authority() {
    for remove_before_error in [false, true] {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let component = RuntimeId::from("jre-legacy");
        let root = cache
            .component_root(component.as_str())
            .expect("managed runtime component root");
        fs::create_dir(&root).expect("canonical runtime root");
        fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
        let source = runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
        let inventory = crate::known_good::runtime_inventory_from_source(&source)
            .expect("authenticated Runtime inventory");
        let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
            .await
            .expect("managed runtime stage");
        let receipt = publish_staged_managed_runtime(staged)
            .await
            .expect("retained managed runtime publish");
        assert!(receipt.quarantine_obligation().is_some_and(|obligation| {
            obligation.observation() == super::ManagedRuntimeQuarantineObservation::Present
        }));

        let failure = if remove_before_error {
            finalize_managed_runtime_commit_with_removed_quarantine_failure_for_test(receipt).await
        } else {
            finalize_managed_runtime_commit_with_failure_for_test(receipt).await
        }
        .expect_err("injected late finalization failure");

        assert_eq!(
            failure.quarantine_obligation().is_some(),
            !remove_before_error
        );
        assert_eq!(
            root.with_file_name("jre-legacy.quarantine").exists(),
            !remove_before_error
        );
        assert!(failure.matches_cache(&cache));
        assert_eq!(failure.component(), &component);
        assert!(failure.matches_known_good_inventory(&inventory));
        assert!(failure.revalidate(&cache, &component).await);

        let contender_source =
            runtime_source_receipt_fixture(&component, &root, b"contender java").await;
        let contender_cache = cache.clone();
        let contender_component = component.clone();
        let mut waiter = tokio::spawn(async move {
            stage_managed_runtime(
                &contender_cache,
                &contender_component,
                contender_source,
                &mut |_| {},
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiter)
                .await
                .is_err(),
            "late finalization failure must retain Runtime publication exclusion"
        );
        drop(failure);
        let staged = waiter
            .await
            .expect("waiting Runtime task")
            .expect("waiting Runtime stage");
        drop(
            publish_staged_managed_runtime(staged)
                .await
                .expect("waiting Runtime publication"),
        );
    }
}

#[tokio::test]
async fn managed_runtime_abandoned_stage_uses_one_recoverable_fixed_slot() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");

    let abandoned_source =
        runtime_source_receipt_fixture(&component, &root, b"abandoned java").await;
    let abandoned = stage_managed_runtime(&cache, &component, abandoned_source, &mut |_| {})
        .await
        .expect("abandoned managed runtime stage");
    let staging_root = abandoned.staging_root_for_test().to_path_buf();
    drop(abandoned);
    assert!(staging_root.exists());
    assert_eq!(
        fs::read(java_executable(&staging_root)).expect("abandoned staged java"),
        b"abandoned java"
    );

    let replacement_source =
        runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
    let replacement = stage_managed_runtime(&cache, &component, replacement_source, &mut |_| {})
        .await
        .expect("replacement managed runtime stage");
    assert_eq!(replacement.staging_root_for_test(), staging_root);
    assert_eq!(
        fs::read(java_executable(&staging_root)).expect("replacement staged java"),
        b"replacement java"
    );
    let staging_slots = fs::read_dir(cache.root())
        .expect("runtime cache entries")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == "jre-legacy.staging")
        .count();
    assert_eq!(staging_slots, 1);

    publish_staged_managed_runtime(replacement)
        .await
        .expect("replacement runtime publish");
    assert!(!staging_root.exists());
    assert_eq!(
        fs::read(
            root.with_file_name("jre-legacy.quarantine")
                .join("sentinel")
        )
        .expect("displaced canonical sentinel"),
        b"original"
    );
}

#[tokio::test]
async fn quarantine_rotation_failure_before_displacement_is_post_effect() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");

    let first_source = runtime_source_receipt_fixture(&component, &root, b"first java").await;
    let first_stage = stage_managed_runtime(&cache, &component, first_source, &mut |_| {})
        .await
        .expect("first managed runtime stage");
    let first_commit = publish_staged_managed_runtime(first_stage)
        .await
        .expect("first retained managed runtime publish");
    let quarantine = first_commit
        .quarantine_root_for_test()
        .expect("retained quarantine");
    drop(first_commit);

    let second_source = runtime_source_receipt_fixture(&component, &root, b"second java").await;
    let second_stage = stage_managed_runtime(&cache, &component, second_source, &mut |_| {})
        .await
        .expect("second managed runtime stage");
    let staging_root = second_stage.staging_root_for_test().to_path_buf();
    let error = publish_staged_managed_runtime_with_rotation_failure_for_test(second_stage)
        .await
        .expect_err("injected quarantine rotation failure");

    let ManagedRuntimeRebuildError::Effect(effect) = error else {
        panic!("quarantine rotation attempt must cross the effect boundary");
    };
    let obligation = effect
        .quarantine_obligation()
        .expect("retained quarantine evidence");
    assert_eq!(obligation.component(), &component);
    assert!(obligation.matches_cache(&cache));
    assert_eq!(
        obligation.observation(),
        super::ManagedRuntimeQuarantineObservation::Present
    );
    assert_eq!(
        fs::read(java_executable(&root)).expect("unchanged canonical java"),
        b"first java"
    );
    assert_eq!(
        fs::read(quarantine.join("sentinel")).expect("retained prior quarantine"),
        b"original"
    );
    assert!(!staging_root.exists());
}

#[tokio::test]
async fn managed_runtime_quarantine_rotation_is_bounded_across_repairs() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");

    let first_source = runtime_source_receipt_fixture(&component, &root, b"first java").await;
    let first_stage = stage_managed_runtime(&cache, &component, first_source, &mut |_| {})
        .await
        .expect("first managed runtime stage");
    let first_commit = publish_staged_managed_runtime(first_stage)
        .await
        .expect("first managed runtime publish");
    let quarantine = first_commit
        .quarantine_root_for_test()
        .expect("first quarantine obligation");
    assert_eq!(
        fs::read(quarantine.join("sentinel")).expect("first displaced sentinel"),
        b"original"
    );
    drop(first_commit);

    let first_java = java_executable(&root);
    assert_eq!(
        fs::read(&first_java).expect("first canonical java"),
        b"first java"
    );
    let second_source = runtime_source_receipt_fixture(&component, &root, b"second java").await;
    let second_stage = stage_managed_runtime(&cache, &component, second_source, &mut |_| {})
        .await
        .expect("second managed runtime stage");
    let second_commit = publish_staged_managed_runtime(second_stage)
        .await
        .expect("second managed runtime publish");

    assert_eq!(
        fs::read(java_executable(&root)).expect("second canonical java"),
        b"second java"
    );
    assert_eq!(
        fs::read(java_executable(&quarantine)).expect("rotated displaced java"),
        b"first java"
    );
    assert_eq!(
        second_commit.quarantine_root_for_test(),
        Some(quarantine.clone())
    );
    let sidecars = fs::read_dir(cache.root())
        .expect("runtime cache entries")
        .filter_map(Result::ok)
        .filter(|entry| {
            matches!(
                entry.file_name().to_str(),
                Some("jre-legacy.staging" | "jre-legacy.quarantine")
            )
        })
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        sidecars,
        vec![std::ffi::OsString::from("jre-legacy.quarantine")]
    );
}

#[tokio::test]
async fn managed_runtime_publication_receipt_holds_the_component_lease() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    let first_source = runtime_source_receipt_fixture(&component, &root, b"first java").await;
    let first_stage = stage_managed_runtime(&cache, &component, first_source, &mut |_| {})
        .await
        .expect("first managed runtime stage");
    let first_receipt = publish_staged_managed_runtime(first_stage)
        .await
        .expect("first managed runtime publish");
    let second_source = runtime_source_receipt_fixture(&component, &root, b"second java").await;
    let second_cache = cache.clone();
    let second_component = component.clone();
    let mut second_stage_task = tokio::spawn(async move {
        stage_managed_runtime(&second_cache, &second_component, second_source, &mut |_| {}).await
    });

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            &mut second_stage_task
        )
        .await
        .is_err(),
        "a second stage must remain pending while the sealed receipt owns the lease"
    );
    drop(first_receipt);

    let second_stage = tokio::time::timeout(std::time::Duration::from_secs(2), second_stage_task)
        .await
        .expect("second stage resumes after receipt drop")
        .expect("second stage task")
        .expect("second managed runtime stage");
    let second_receipt = publish_staged_managed_runtime(second_stage)
        .await
        .expect("second managed runtime publish");
    assert!(second_receipt.revalidate(&cache, &component).await);
}

#[tokio::test]
async fn managed_runtime_admission_rejects_unsafe_sizes_before_effects() {
    let (oversized_url, oversized_requests) =
        serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let oversized = ComponentManifest {
        files: HashMap::from([(
            runtime_java_manifest_path(),
            downloadable_manifest_file(
                &oversized_url,
                (128 << 20) + 1,
                "0000000000000000000000000000000000000000",
            ),
        )]),
    };
    assert_managed_manifest_rejection_preserves_state(oversized, oversized_requests).await;

    let (missing_size_url, missing_size_requests) =
        serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let mut missing_size = downloadable_manifest_file(
        &missing_size_url,
        1,
        "0000000000000000000000000000000000000000",
    );
    missing_size
        .downloads
        .as_mut()
        .expect("download proof")
        .raw
        .as_mut()
        .expect("raw proof")
        .size = None;
    let missing_size = ComponentManifest {
        files: HashMap::from([(runtime_java_manifest_path(), missing_size)]),
    };
    assert_managed_manifest_rejection_preserves_state(missing_size, missing_size_requests).await;
}

#[tokio::test]
async fn managed_runtime_admission_rejects_case_aliases_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let file = downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000");
    let manifest = ComponentManifest {
        files: HashMap::from([
            ("bin/java.exe".to_string(), file.clone()),
            ("BIN/JAVA.EXE".to_string(), file),
        ]),
    };

    assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
}

#[tokio::test]
async fn managed_runtime_admission_rejects_explicit_folded_prefix_alias_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let manifest = ComponentManifest {
        files: HashMap::from([
            ("bin".to_string(), manifest_file("directory")),
            (
                "BIN/java".to_string(),
                downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000"),
            ),
        ]),
    };

    assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
}

#[tokio::test]
async fn managed_runtime_admission_rejects_implicit_folded_prefix_alias_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let file = downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000");
    let manifest = ComponentManifest {
        files: HashMap::from([
            ("bin/a".to_string(), file.clone()),
            ("BIN/b".to_string(), file),
        ]),
    };

    assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
}

#[tokio::test]
async fn managed_runtime_admission_rejects_case_mismatched_link_target_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let manifest = ComponentManifest {
        files: HashMap::from([
            (
                "legal/base/LICENSE".to_string(),
                downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000"),
            ),
            (
                "legal/module/LICENSE".to_string(),
                manifest_link("../BASE/LICENSE"),
            ),
        ]),
    };

    assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
}

#[tokio::test]
async fn managed_runtime_admission_rejects_dangling_link_target_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let manifest = ComponentManifest {
        files: HashMap::from([
            (
                "bin/java".to_string(),
                downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000"),
            ),
            (
                "legal/module/LICENSE".to_string(),
                manifest_link("../base/LICENSE"),
            ),
        ]),
    };

    assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
}

#[tokio::test]
async fn managed_runtime_admission_rejects_backslash_link_target_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let manifest = ComponentManifest {
        files: HashMap::from([
            (
                "legal/base/LICENSE".to_string(),
                downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000"),
            ),
            (
                "legal/module/LICENSE".to_string(),
                manifest_link(r"..\base\LICENSE"),
            ),
        ]),
    };

    assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
}

#[tokio::test]
async fn managed_runtime_admission_rejects_link_loop_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let manifest = ComponentManifest {
        files: HashMap::from([
            (
                "bin/java".to_string(),
                downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000"),
            ),
            ("legal/loop".to_string(), manifest_link("loop")),
        ]),
    };

    assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
}

#[test]
fn runtime_manifest_admission_rejects_portable_and_runtime_owned_collisions() {
    let file = downloadable_manifest_file(
        "https://example.invalid/java",
        1,
        "0000000000000000000000000000000000000000",
    );
    let manifests = [
        ComponentManifest {
            files: HashMap::from([
                ("bin/java".to_string(), file.clone()),
                ("BIN/JAVA".to_string(), file.clone()),
            ]),
        },
        ComponentManifest {
            files: HashMap::from([
                ("bin/java".to_string(), file.clone()),
                ("bin/java.axial-tmp".to_string(), file.clone()),
            ]),
        },
        ComponentManifest {
            files: HashMap::from([(".AXIAL-READY".to_string(), file)]),
        },
    ];

    for manifest in manifests {
        let error = validate_ephemeral_processor_manifest_for_test(&manifest, 1)
            .expect_err("filesystem alias must be rejected during source admission");
        assert!(matches!(
            error,
            JavaRuntimeLookupError::RuntimeSource(failure)
                if failure.kind() == RuntimeSourceFailureKind::PolicyRejected
        ));
    }
}

#[test]
fn runtime_manifest_admission_accepts_declared_forward_slash_link_target() {
    let manifest = ComponentManifest {
        files: HashMap::from([
            (
                "legal/base/LICENSE".to_string(),
                downloadable_manifest_file(
                    "https://example.invalid/license",
                    1,
                    "0000000000000000000000000000000000000000",
                ),
            ),
            (
                "legal/module/LICENSE".to_string(),
                manifest_link("../base/LICENSE"),
            ),
            ("legal/module/base".to_string(), manifest_link("../base")),
        ]),
    };

    validate_ephemeral_processor_manifest_for_test(&manifest, 1)
        .expect("declared and implicit portable link targets should be admitted");
}

#[tokio::test]
async fn managed_runtime_admission_rejects_over_entry_manifest_before_effects() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let mut files = (0..4096)
        .map(|index| (format!("entry-{index}"), manifest_file("directory")))
        .collect::<HashMap<_, _>>();
    files.insert(
        runtime_java_manifest_path(),
        downloadable_manifest_file(&url, 1, "0000000000000000000000000000000000000000"),
    );

    assert_managed_manifest_rejection_preserves_state(ComponentManifest { files }, requests).await;
}

#[tokio::test]
async fn managed_runtime_stage_rejects_extra_tree_entries_before_displacement() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
    let source = runtime_source_receipt_fixture(&component, &root, b"replacement java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");
    let staging_root = staged.staging_root_for_test().to_path_buf();
    fs::write(staging_root.join("undeclared"), b"extra").expect("undeclared staged file");

    let error = publish_staged_managed_runtime(staged)
        .await
        .expect_err("extra staged entry must fail exact verification");

    assert!(matches!(error, ManagedRuntimeRebuildError::Preparation(_)));
    assert_eq!(
        fs::read(root.join("sentinel")).expect("untouched sentinel"),
        b"original"
    );
    assert!(!staging_root.exists());
}

#[tokio::test]
async fn managed_runtime_commit_revalidation_rejects_ready_marker_tampering() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    let source = runtime_source_receipt_fixture(&component, &root, b"authenticated java").await;
    let staged = stage_managed_runtime(&cache, &component, source, &mut |_| {})
        .await
        .expect("managed runtime stage");
    let receipt = publish_staged_managed_runtime(staged)
        .await
        .expect("managed runtime publish");
    assert!(receipt.revalidate(&cache, &component).await);

    fs::write(root.join(".axial-ready"), b"nope").expect("tamper ready marker");

    assert!(!receipt.revalidate(&cache, &component).await);
}

async fn runtime_source_receipt_fixture(
    component: &RuntimeId,
    runtime_root: &Path,
    java_bytes: &[u8],
) -> RuntimeSourceReceipt {
    runtime_source_receipt_fixture_with_download_count(component, runtime_root, java_bytes, 1).await
}

async fn runtime_source_receipt_fixture_with_download_count(
    component: &RuntimeId,
    runtime_root: &Path,
    java_bytes: &[u8],
    download_count: usize,
) -> RuntimeSourceReceipt {
    let java_url = if download_count == 1 {
        serve_runtime_download(java_bytes.to_vec()).await
    } else {
        serve_runtime_retry_responses(vec![(200, java_bytes.to_vec()); download_count])
            .await
            .0
    };
    let java_relative_path = java_executable(runtime_root)
        .strip_prefix(runtime_root)
        .expect("java path under runtime root")
        .to_string_lossy()
        .replace('\\', "/");
    let mut java_file =
        downloadable_manifest_file(&java_url, java_bytes.len() as u64, &sha1_hex(java_bytes));
    java_file.executable = true;
    let component_manifest = ComponentManifest {
        files: HashMap::from([(java_relative_path, java_file)]),
    };
    let manifest_bytes =
        serde_json::to_vec(&component_manifest).expect("component manifest fixture");
    let manifest_url = serve_runtime_json(200, manifest_bytes.clone(), None).await;
    acquire_runtime_source_for_test(
        component.clone(),
        RuntimeDownloadManifest {
            url: manifest_url,
            sha1: sha1_hex(&manifest_bytes),
            size: manifest_bytes.len() as u64,
        },
    )
    .await
    .expect("authenticated runtime source fixture")
}

fn runtime_java_manifest_path() -> String {
    java_executable(Path::new("runtime"))
        .strip_prefix("runtime")
        .expect("runtime Java remains below the fixture root")
        .to_string_lossy()
        .replace('\\', "/")
}

async fn assert_managed_manifest_rejection_preserves_state(
    manifest: ComponentManifest,
    requests: Arc<AtomicUsize>,
) {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    let staging_root = root.with_file_name("jre-legacy.staging");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"canonical").expect("canonical sentinel");
    fs::create_dir(&staging_root).expect("existing fixed staging root");
    fs::write(staging_root.join("sentinel"), b"staging").expect("staging sentinel");
    let source = authenticated_runtime_source_from_manifest_for_test(component.clone(), manifest)
        .expect("authenticated invalid runtime source fixture");
    let mut events = Vec::new();

    let result =
        stage_managed_runtime(&cache, &component, source, &mut |event| events.push(event)).await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure)) if failure.component() == &component
    ));
    tokio::task::yield_now().await;
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(events.is_empty());
    assert_eq!(
        fs::read(root.join("sentinel")).expect("unchanged canonical sentinel"),
        b"canonical"
    );
    assert_eq!(
        fs::read(staging_root.join("sentinel")).expect("unchanged staging sentinel"),
        b"staging"
    );
    assert!(!runtime_install_lock_file_path(&root).exists());
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn sealed_runtime_fixture_replaces_only_active_runtime_projection() {
    use crate::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, KnownGoodRoot, TestKnownGoodEntry,
        TestKnownGoodIntegrity, TestKnownGoodRoot,
    };

    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let active = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
        root: TestKnownGoodRoot::Versions,
        path: "fixture/fixture.json".to_string(),
        kind: KnownGoodArtifactKind::VersionMetadata,
        integrity: TestKnownGoodIntegrity::File { size: 1 },
    }])
    .expect("active known-good fixture");
    let receipt = super::rebuild_managed_runtime_fixture_for_test(&cache, component.clone())
        .await
        .expect("sealed runtime rebuild fixture");

    let merged = receipt
        .replace_known_good_runtime_projection(&active)
        .expect("replace sealed runtime projection");

    assert!(receipt.matches_known_good_inventory(&merged));
    assert!(merged.entries().iter().any(|entry| {
        matches!(entry.root(), KnownGoodRoot::Versions)
            && entry.path().as_str() == "fixture/fixture.json"
    }));
    assert!(merged.entries().iter().any(|entry| matches!(
        entry.root(),
        KnownGoodRoot::ManagedRuntime { component: observed }
            if observed.as_str() == component.as_str()
    )));

    let foreign_active = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
        root: TestKnownGoodRoot::ManagedRuntime {
            component: "java-runtime-gamma".to_string(),
        },
        path: "bin/java".to_string(),
        kind: KnownGoodArtifactKind::RuntimeExecutable,
        integrity: TestKnownGoodIntegrity::File { size: 1 },
    }])
    .expect("foreign active Runtime projection fixture");
    assert!(
        receipt
            .replace_known_good_runtime_projection(&foreign_active)
            .is_err(),
        "a sealed receipt must not retain a foreign active Runtime projection"
    );
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
        &test_runtime_component(),
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
fn runtime_manifest_admission_rejects_checked_byte_overflow() {
    let manifest = ComponentManifest {
        files: HashMap::from([(
            "bin/java".to_string(),
            downloadable_manifest_file(
                "https://example.invalid/java",
                1,
                "0000000000000000000000000000000000000000",
            ),
        )]),
    };

    let error = validate_ephemeral_processor_manifest_for_test(&manifest, u64::MAX)
        .expect_err("manifest admission total must use checked arithmetic");
    assert!(matches!(
        error,
        JavaRuntimeLookupError::RuntimeSource(failure)
            if failure.kind() == RuntimeSourceFailureKind::PolicyRejected
                && failure.detail().contains("overflowed")
    ));
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
        &test_runtime_component(),
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
        &test_runtime_component(),
        &client,
        &url,
        &temp_path,
        expected(Some(6), None),
        "bin/java",
    )
    .await;

    assert!(matches!(
        &result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::IntegrityMismatch
    ));
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
        &test_runtime_component(),
        &client,
        &url,
        &temp_path,
        expected(Some(5), None),
        "bin/java",
    )
    .await;

    assert!(matches!(
        &result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::IntegrityMismatch
    ));
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

    let result = install_runtime_manifest_file(
        &test_runtime_component(),
        runtime_download_client().clone(),
        &root,
        "bin/java",
        file,
    )
    .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::MetadataInvalid
                && failure.detail().contains("missing checksum proof")
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

    install_runtime_manifest_file(
        &test_runtime_component(),
        runtime_download_client().clone(),
        &root,
        "bin/java",
        file,
    )
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

    let result = install_runtime_manifest_file(
        &test_runtime_component(),
        runtime_download_client().clone(),
        &root,
        "bin/java",
        file,
    )
    .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::MetadataInvalid
                && failure.detail().contains("missing checksum proof")
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
        &test_runtime_component(),
        &client,
        &url,
        &temp_path,
        expected(Some(5), None),
        "bin/java",
    )
    .await;

    assert!(matches!(
        &result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::IntegrityMismatch
    ));
    assert!(!temp_path.exists());
    assert!(
        result
            .expect_err("oversized stream should fail")
            .to_string()
            .contains("runtime file bin/java size mismatch: expected 5, got 6")
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn ready_managed_runtime_paths_reuse_structural_install_without_source_refresh() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = test_runtime_component();
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime root");
    let java_bytes = b"ready managed java";
    let manifest = persisted_java_manifest(&root, "https://example.invalid/java", java_bytes);
    write_persisted_runtime_source(&root, &manifest);
    let java = java_executable(&root);
    fs::create_dir_all(java.parent().expect("java parent")).expect("java parent dir");
    fs::write(&java, java_bytes).expect("java fixture");
    make_executable(&java);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        b"provider refresh must not run",
    )
    .expect("replace persisted source proof after structural admission");
    let admissions = AtomicUsize::new(0);

    for requested_override in ["", component.as_str()] {
        let mut events = Vec::new();
        let ensured = ensure_runtime_with_persisted_manifest_for_test(
            &cache,
            &JavaVersion {
                component: component.as_str().to_string(),
                major_version: 21,
            },
            requested_override,
            false,
            None,
            || {
                admissions.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ManagedRuntimeMutationRefused>(())
            },
            |event| events.push(event),
        )
        .await
        .expect("ready runtime ensure");

        assert_eq!(admissions.load(Ordering::SeqCst), 0);
        assert_eq!(ensured.effective.install_state, RuntimeInstallState::Ready);
        assert_eq!(
            events,
            vec![RuntimeEnsureEvent::ManagedRuntimeReady {
                component: component.as_str().to_string(),
            }]
        );
    }
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn missing_managed_runtime_admits_once_and_holds_permit_through_ready() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = test_runtime_component();
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime root");
    let java_bytes = b"installed managed java".to_vec();
    let java_url = serve_runtime_download(java_bytes.clone()).await;
    let manifest = persisted_java_manifest(&root, &java_url, &java_bytes);
    write_persisted_runtime_source(&root, &manifest);
    let admissions = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let admission_count = Arc::clone(&admissions);
    let admitted_active = Arc::clone(&active);
    let observed_active = Arc::clone(&active);
    let mut events = Vec::new();

    let ensured = ensure_runtime_with_persisted_manifest_for_test(
        &cache,
        &JavaVersion {
            component: component.as_str().to_string(),
            major_version: 21,
        },
        "",
        false,
        None,
        move || {
            assert_eq!(admission_count.fetch_add(1, Ordering::SeqCst), 0);
            assert_eq!(admitted_active.swap(1, Ordering::SeqCst), 0);
            Ok(RuntimeMutationPermitProbe {
                active: admitted_active,
            })
        },
        |event| {
            assert_eq!(observed_active.load(Ordering::SeqCst), 1);
            events.push(event);
        },
    )
    .await
    .expect("missing runtime install");

    assert_eq!(admissions.load(Ordering::SeqCst), 1);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(ensured.effective.install_state, RuntimeInstallState::Ready);
    assert_eq!(
        fs::read(java_executable(&root)).expect("installed java"),
        java_bytes
    );
    assert!(matches!(
        events.first(),
        Some(RuntimeEnsureEvent::DownloadingManagedRuntime { component: observed })
            if observed == component.as_str()
    ));
    assert!(matches!(
        events.last(),
        Some(RuntimeEnsureEvent::ManagedRuntimeReady { component: observed })
            if observed == component.as_str()
    ));
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn refused_managed_runtime_admission_has_no_install_effects() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = test_runtime_component();
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime root");
    let java_bytes = b"must not download";
    let manifest = persisted_java_manifest(&root, "http://127.0.0.1:9/java", java_bytes);
    write_persisted_runtime_source(&root, &manifest);
    let original_proof = component_manifest_proof_bytes(&manifest).expect("manifest proof");
    let admissions = AtomicUsize::new(0);
    let mut events = Vec::new();

    let error = ensure_runtime_with_persisted_manifest_for_test(
        &cache,
        &JavaVersion {
            component: component.as_str().to_string(),
            major_version: 21,
        },
        "",
        false,
        None,
        || {
            admissions.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(ManagedRuntimeMutationRefused)
        },
        |event| events.push(event),
    )
    .await
    .expect_err("mutation refusal must stop runtime install");

    assert!(matches!(
        error,
        JavaRuntimeLookupError::ManagedMutationRefused
    ));
    assert_eq!(admissions.load(Ordering::SeqCst), 1);
    assert!(events.is_empty());
    assert!(!java_executable(&root).exists());
    assert!(!root.join(".axial-ready").exists());
    assert_eq!(
        fs::read(root.join(".axial-runtime-manifest.json")).expect("preserved proof"),
        original_proof
    );
    let mut cache_entries = fs::read_dir(cache.root())
        .expect("runtime cache entries")
        .map(|entry| entry.expect("runtime cache entry").file_name())
        .collect::<Vec<_>>();
    cache_entries.sort();
    assert_eq!(
        cache_entries,
        vec![std::ffi::OsString::from(component.as_str())]
    );
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
            &test_runtime_component(),
            spawned_client,
            &spawned_root,
            "bin/java",
            spawned_file,
        ))
        .await
    };

    assert!(
        std::mem::size_of_val(&fetch_runtime_file(
            &test_runtime_component(),
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
            &test_runtime_component(),
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
            || Ok(()),
            |_| {},
        )) < 4096,
        "managed-runtime ensure future should stay small"
    );
}

#[cfg(feature = "test-support")]
fn persisted_java_manifest(root: &Path, url: &str, java_bytes: &[u8]) -> ComponentManifest {
    let relative_java = java_executable(root)
        .strip_prefix(root)
        .expect("java under managed runtime root")
        .to_string_lossy()
        .replace('\\', "/");
    let mut file = downloadable_manifest_file(url, java_bytes.len() as u64, &sha1_hex(java_bytes));
    file.executable = true;
    ComponentManifest {
        files: HashMap::from([(relative_java, file)]),
    }
}

#[cfg(feature = "test-support")]
fn write_persisted_runtime_source(root: &Path, manifest: &ComponentManifest) {
    fs::create_dir_all(root).expect("managed runtime root");
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        component_manifest_proof_bytes(manifest).expect("canonical runtime manifest proof"),
    )
    .expect("persisted runtime source");
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
