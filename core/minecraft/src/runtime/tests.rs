use super::file_download::component_manifest_link_target_path;
use super::{
    ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
    ComponentManifestFile, JavaRuntimeLookupError, MachOArm64Compatibility, ManagedRuntimeCache,
    ManagedRuntimeComponent, ManagedRuntimeRebuildError, RosettaRuntimeDecision,
    RuntimeDownloadActual, RuntimeDownloadEvidence, RuntimeDownloadIntegrityError,
    RuntimeDownloadManifest, RuntimeEnsureEvent, RuntimeId, RuntimeManifest,
    RuntimeMaterializationCancellation, RuntimeSourceFailure, RuntimeSourceFailureKind,
    RuntimeSourceReceipt, acquire_runtime_source_for_test,
    authenticated_runtime_source_from_manifest_for_test,
    block_runtime_before_publication_claim_for_test, block_runtime_decompression_for_test,
    block_runtime_publication_for_test, component_manifest_destination,
    component_manifest_proof_bytes, discard_staged_managed_runtime,
    fetch_runtime_manifest_bytes_for_test, install_runtime_manifest_file,
    install_runtime_manifest_files, java_executable, java_executable_for_os,
    materialize_missing_runtime_source, parse_mach_o_arm64_compatibility,
    publish_staged_managed_runtime, publish_staged_managed_runtime_and_finalize,
    publish_staged_managed_runtime_with_displacement_failure_for_test,
    publish_staged_managed_runtime_with_finalization_failure_for_test,
    publish_staged_managed_runtime_with_promotion_failure_for_test,
    publish_staged_managed_runtime_with_restoration_failure_for_test,
    publish_staged_managed_runtime_with_rotation_failure_for_test,
    rosetta_requirement_for_managed_runtime, runtime_file_download_concurrency_for,
    runtime_materialization_control, runtime_os_arch_for,
    runtime_publication_lock_available_for_test, runtime_windows_verbatim_path_string,
    select_runtime_manifest, stage_managed_runtime, stage_managed_runtime_until_cancelled,
    validate_ephemeral_processor_manifest_for_test, validate_runtime_file_source_urls_for_test,
    verify_runtime_download,
};
use crate::JavaVersion;
use crate::http::test_server::{Server, response, serve};
use sha1::{Digest as _, Sha1};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn expected(size: Option<u64>, sha1: Option<&str>) -> RuntimeDownloadEvidence {
    RuntimeDownloadEvidence {
        size,
        sha1: sha1.map(str::to_string),
    }
}

fn test_runtime_component() -> RuntimeId {
    RuntimeId::from("java-runtime-delta")
}

fn runtime_absence_fixture() -> (
    tempfile::TempDir,
    axial_fs::RootSession,
    axial_fs::Directory,
    ManagedRuntimeCache,
) {
    let temporary = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let path = temporary.path().join("runtime");
    fs::create_dir(&path).unwrap();
    let session = match axial_fs::RootSession::acquire(temporary.path()) {
        axial_fs::RootSessionAcquireOutcome::Acquired(session) => session,
        outcome => panic!("could not acquire fixture: {outcome:?}"),
    };
    let directory = session
        .root()
        .unwrap()
        .open_directory(&axial_fs::LeafName::new("runtime").unwrap())
        .unwrap();
    let cache = ManagedRuntimeCache::from_directory(directory.clone(), path).unwrap();
    (temporary, session, directory, cache)
}

fn read_fixture_file(path: &Path) -> std::io::Result<([u8; 64], usize)> {
    let mut file = fs::File::open(path)?;
    let mut bytes = [0; 64];
    let mut length = 0;
    while length < bytes.len() {
        let read = std::io::Read::read(&mut file, &mut bytes[length..])?;
        if read == 0 {
            break;
        }
        length += read;
    }
    Ok((bytes, length))
}

#[test]
fn runtime_absence_retains_known_empty_component_observation() {
    let (_temporary, session, directory, cache) = runtime_absence_fixture();
    for unknown in ["", "unknown", "../java-runtime-delta"] {
        assert!(cache.observe_missing_component(unknown).is_err());
    }
    let absence = cache
        .observe_missing_component("java-runtime-delta")
        .unwrap()
        .expect("known component is absent");
    absence.revalidate().unwrap();
    cache.settle().unwrap();
    drop(cache);
    absence.revalidate().unwrap();
    drop((absence, directory));
    assert!(matches!(
        session.revoke(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

#[test]
fn runtime_absence_preserves_existing_canonical_and_sidecar_entries() {
    for (name, is_directory) in [
        ("java-runtime-delta", true),
        ("java-runtime-delta", false),
        ("java-runtime-delta.staging", true),
        ("java-runtime-delta.staging", false),
        ("java-runtime-delta.quarantine", true),
        ("java-runtime-delta.quarantine", false),
    ] {
        let (_temporary, session, directory, cache) = runtime_absence_fixture();
        let absence = cache
            .observe_missing_component("java-runtime-delta")
            .unwrap()
            .unwrap();
        let path = cache.root().join(name);
        let canary = if is_directory {
            fs::create_dir(&path).unwrap();
            path.join("preserved")
        } else {
            path.clone()
        };
        fs::write(&canary, b"preserved runtime conflict").unwrap();
        let observation = cache.observe_missing_component("java-runtime-delta");
        if name == "java-runtime-delta" && is_directory {
            assert!(observation.unwrap().is_none());
            // Existing runtime selection, not absence, owns its sidecar policy.
            let sidecar = cache.root().join("java-runtime-delta.staging");
            fs::write(&sidecar, b"preserved existing sidecar").unwrap();
            assert!(
                cache
                    .observe_missing_component("java-runtime-delta")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(fs::read(sidecar).unwrap(), b"preserved existing sidecar");
        } else {
            assert!(observation.is_err(), "{name}");
        }
        assert!(absence.revalidate().is_err());
        assert_eq!(fs::read(canary).unwrap(), b"preserved runtime conflict");
        cache.settle().unwrap();
        drop((absence, cache, directory));
        assert!(matches!(
            session.revoke(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
    }
}

#[test]
fn runtime_absence_does_not_refresh_after_observed_namespace_churn() {
    let (_temporary, session, directory, cache) = runtime_absence_fixture();
    let original = directory.revision().unwrap();
    let absence = cache
        .observe_missing_component("java-runtime-delta")
        .unwrap()
        .unwrap();
    let component = cache.root().join("java-runtime-delta");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        fs::create_dir(&component).unwrap();
        fs::remove_dir(&component).unwrap();
        if directory.revision().unwrap() != original {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native revision did not change"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(directory.validate_revision(&original).is_err());
    assert!(
        cache
            .admit_component("java-runtime-delta")
            .unwrap()
            .is_none()
    );
    assert!(absence.revalidate().is_err());
    assert!(absence.revalidate().is_err());
    let fresh = cache
        .observe_missing_component("java-runtime-delta")
        .unwrap()
        .unwrap();
    fresh.revalidate().unwrap();
    cache.settle().unwrap();
    drop((fresh, absence, cache, directory));
    assert!(matches!(
        session.revoke(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

#[test]
fn runtime_absence_refuses_portable_aliases_without_changing_them() {
    for name in [
        "JAVA-RUNTIME-DELTA",
        "JAVA-RUNTIME-DELTA.STAGING",
        "JAVA-RUNTIME-DELTA.QUARANTINE",
    ] {
        let (_temporary, session, directory, cache) = runtime_absence_fixture();
        let path = cache.root().join(name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("preserved"), b"preserved alias").unwrap();
        assert!(
            cache
                .observe_missing_component("java-runtime-delta")
                .is_err()
        );
        assert_eq!(
            fs::read(path.join("preserved")).unwrap(),
            b"preserved alias"
        );
        cache.settle().unwrap();
        drop((cache, directory));
        assert!(matches!(
            session.revoke(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
    }
}

#[cfg(unix)]
#[test]
fn runtime_absence_refuses_replaced_cache_root() {
    let (temporary, session, directory, cache) = runtime_absence_fixture();
    fs::write(cache.root().join("preserved"), b"preserved root").unwrap();
    let absence = cache
        .observe_missing_component("java-runtime-delta")
        .unwrap()
        .unwrap();
    let root = cache.root().to_path_buf();
    let retained = temporary.path().join("retained-runtime");
    fs::rename(&root, &retained).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(absence.revalidate().is_err());
    assert!(
        cache
            .observe_missing_component("java-runtime-delta")
            .is_err()
    );
    assert_eq!(
        fs::read(retained.join("preserved")).unwrap(),
        b"preserved root"
    );
    assert!(fs::read_dir(&root).unwrap().next().is_none());
    fs::remove_dir(&root).unwrap();
    fs::rename(&retained, &root).unwrap();
    cache.settle().unwrap();
    drop((absence, cache, directory));
    assert!(matches!(
        session.revoke(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

fn admit_runtime_component(
    cache: &ManagedRuntimeCache,
    component: &str,
) -> ManagedRuntimeComponent {
    cache
        .admit_component(component)
        .expect("managed runtime admission")
        .expect("managed runtime component")
}

fn runtime_launch_receipt_test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

#[test]
fn component_manifest_destination_admits_only_safe_portable_paths() {
    let temp_dir = Path::new("runtime-temp");
    assert_eq!(
        component_manifest_destination(&test_runtime_component(), temp_dir, "bin/java").unwrap(),
        temp_dir.join("bin").join("java")
    );
    let absolute_path = if cfg!(windows) {
        r"\Windows\System32"
    } else {
        "/etc/passwd"
    };
    let overlong = "a".repeat(crate::portable_path::MAX_PORTABLE_FILE_NAME_BYTES + 1);
    for relative_path in [
        "bin/../java",
        absolute_path,
        "C:/Windows/System32",
        r"C:\Windows\System32",
        "bin/NUL.txt",
        "bin/java.",
        "bin/java ",
        "bin/ja*va",
        "bin/ja\0va",
        "bin/cafe\u{301}",
        overlong.as_str(),
    ] {
        let message = unsafe_manifest_path_message(component_manifest_destination(
            &test_runtime_component(),
            temp_dir,
            relative_path,
        ));
        assert!(
            message.contains("unsafe runtime manifest path") && !message.contains("runtime-temp"),
            "{relative_path:?}: {message}"
        );
    }
}

#[test]
fn component_manifest_link_target_rejects_nonportable_named_segments() {
    for target in [
        "../NUL.txt",
        "../license.",
        "../license ",
        "../li*ense",
        "../cafe\u{301}",
    ] {
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

#[tokio::test]
async fn runtime_manifest_install_counts_files_and_links_in_progress() {
    let root = unique_temp_root("axial-runtime-progress-test");
    let license = b"license";
    let size = license.len() as u64;
    let mut files = HashMap::from([
        ("legal".to_string(), manifest_file("directory")),
        (
            "legal/java.base/LICENSE".to_string(),
            downloadable_manifest_file(&serve_body(license).await, size, &sha1_hex(license)),
        ),
    ]);
    if cfg!(unix) {
        files.insert(
            "legal/java.compiler/LICENSE".to_string(),
            manifest_link("../java.base/LICENSE"),
        );
    }
    let total = files.len() - 1;
    let mut events = Vec::new();

    install_runtime_manifest_files(&test_runtime_component(), &root, files, &mut |event| {
        events.push(event);
    })
    .await
    .expect("runtime manifest files");

    #[cfg(unix)]
    assert_eq!(
        fs::read_link(root.join("legal/java.compiler/LICENSE")).expect("runtime link"),
        PathBuf::from("../java.base/LICENSE")
    );
    let progress = events
        .iter()
        .map(|event| match event {
            RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
                current,
                total,
                bytes_done,
                bytes_total,
                ..
            } => (*current, *total, *bytes_done, *bytes_total),
            other => panic!("unexpected runtime event {other:?}"),
        })
        .collect::<Vec<_>>();
    let expected = (0..=total)
        .map(|current| (current, total, if current == 0 { 0 } else { size }, size))
        .collect::<Vec<_>>();
    assert_eq!(progress, expected);
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_manifest_link_rejects_target_escape() {
    let root = unique_temp_root("axial-runtime-link-escape-test");
    let result = install_runtime_manifest_file(
        &test_runtime_component(),
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
async fn runtime_manifest_json_fetch_classifies_permanent_http_status_as_metadata_invalid() {
    let url = serve(vec![response("404 Not Found", &[], b"missing")])
        .await
        .url("/runtime.json");

    let error = fetch_runtime_manifest_bytes_for_test(&url)
        .await
        .expect_err("permanent HTTP error should fail");

    let failure = expect_runtime_source_failure(error, RuntimeSourceFailureKind::MetadataInvalid);
    assert!(failure.detail().contains("HTTP 404"), "{failure:?}");
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
    let url = serve_body(&bytes).await;
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
async fn missing_runtime_source_preserves_canonical_and_sidecar_conflicts() {
    for conflict in ["canonical", "staging", "quarantine"] {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let component = RuntimeId::from("jre-legacy");
        let root = cache.component_root(component.as_str()).unwrap();
        let source = runtime_source_receipt_fixture(&component, b"original java").await;
        let repeated = authenticated_runtime_source_from_manifest_for_test(
            component.clone(),
            source.manifest().clone(),
        )
        .expect("same authenticated source");
        assert_eq!(source.bytes(), repeated.bytes());
        let java_version = JavaVersion {
            component: component.as_str().to_string(),
            major_version: 8,
        };
        let (_cancel, mut control) = runtime_materialization_control();
        let receipt = materialize_missing_runtime_source(
            &cache,
            &java_version,
            source,
            &mut |_| {},
            &mut control,
        )
        .await
        .expect("initial runtime publication")
        .expect("published runtime receipt");
        assert!(receipt.revalidate(&cache, &component).await);
        drop(receipt);

        let preserved = if conflict == "canonical" {
            java_executable(&root)
        } else {
            let sidecar = root.with_file_name(format!("jre-legacy.{conflict}"));
            fs::create_dir(&sidecar).unwrap();
            sidecar.join("unknown")
        };
        fs::write(&preserved, b"foreign bytes").unwrap();
        let (_cancel, mut control) = runtime_materialization_control();
        let result = materialize_missing_runtime_source(
            &cache,
            &java_version,
            repeated,
            &mut |_| {},
            &mut control,
        )
        .await;
        assert!(matches!(
            result,
            Err(ManagedRuntimeRebuildError::Preparation(
                JavaRuntimeLookupError::ManagedMutationRefused
            ))
        ));
        assert_eq!(fs::read(&preserved).unwrap(), b"foreign bytes");
        assert_eq!(
            fs::read(java_executable(&root)).unwrap(),
            if conflict == "canonical" {
                &b"foreign bytes"[..]
            } else {
                &b"original java"[..]
            }
        );
        for suffix in ["staging", "quarantine"] {
            assert_eq!(
                root.with_file_name(format!("jre-legacy.{suffix}")).exists(),
                conflict == suffix
            );
        }
        assert_eq!(
            admit_runtime_component(&cache, component.as_str()).contents_verified(),
            conflict != "canonical"
        );
        assert!(runtime_publication_lock_available_for_test(
            &cache, &component
        ));
        cache.settle().expect("refusal leaves no owned effects");
    }
}

#[tokio::test]
async fn missing_runtime_source_concurrent_producers_publish_once_and_reuse() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache.component_root(component.as_str()).unwrap();
    let source = runtime_source_receipt_fixture(&component, b"concurrent java").await;
    let repeated = authenticated_runtime_source_from_manifest_for_test(
        component.clone(),
        source.manifest().clone(),
    )
    .expect("same authenticated source");
    assert_eq!(source.bytes(), repeated.bytes());
    let java_version = JavaVersion {
        component: component.as_str().to_string(),
        major_version: 8,
    };
    let produce = |source| {
        let (cache, component, java_version) = (&cache, &component, &java_version);
        async move {
            let (_cancel, mut control) = runtime_materialization_control();
            let mut downloads = 0;
            let receipt = materialize_missing_runtime_source(
                cache,
                java_version,
                source,
                &mut |event| {
                    if matches!(event, RuntimeEnsureEvent::DownloadingManagedRuntime { .. }) {
                        downloads += 1;
                    }
                },
                &mut control,
            )
            .await
            .expect("concurrent runtime materialization")
            .expect("published or reused receipt");
            let valid = receipt.revalidate(cache, component).await;
            drop(receipt);
            (valid, downloads)
        }
    };
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(produce(source), produce(repeated))
    })
    .await
    .expect("both producers settle");

    assert!(first.0 && second.0);
    assert_eq!(first.1 + second.1, 1);
    assert_eq!(
        fs::read(java_executable(&root)).unwrap(),
        b"concurrent java"
    );
    assert!(admit_runtime_component(&cache, component.as_str()).contents_verified());
    assert!(!root.with_file_name("jre-legacy.staging").exists());
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
    assert!(runtime_publication_lock_available_for_test(
        &cache, &component
    ));
    cache
        .settle()
        .expect("both producers released owned effects");
}

#[tokio::test]
async fn missing_runtime_source_cancellation_before_publication_cleans_only_owned_stage() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache.component_root(component.as_str()).unwrap();
    let source = runtime_source_receipt_fixture(&component, b"cancelled java").await;
    let unrelated = cache.root().join("unrelated");
    fs::write(&unrelated, b"preserve").unwrap();
    let mut gate = block_runtime_before_publication_claim_for_test(&root);
    let (cancel, mut control) = runtime_materialization_control();
    let task_cache = cache.clone();
    let task = tokio::spawn(async move {
        materialize_missing_runtime_source(
            &task_cache,
            &JavaVersion {
                component: "jre-legacy".into(),
                major_version: 8,
            },
            source,
            &mut |_| {},
            &mut control,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        gate.wait_until_reached(),
    )
    .await
    .expect("actual stage reaches publication claim");
    let staging = root.with_file_name("jre-legacy.staging");
    let staged_bytes = fs::read(java_executable(&staging));
    let held = !runtime_publication_lock_available_for_test(&cache, &component);
    let cancellation = cancel.cancel_before_publication();
    gate.release();
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), task)
        .await
        .expect("cancelled producer drains")
        .expect("producer joins")
        .expect("owned stage cleanup settles");

    assert_eq!(staged_bytes.unwrap(), b"cancelled java");
    assert!(held);
    assert_eq!(cancellation, RuntimeMaterializationCancellation::Cancelled);
    assert!(result.is_none());
    assert!(cache.admit_component(component.as_str()).unwrap().is_none());
    assert!(!root.exists());
    assert!(!staging.exists());
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
    assert_eq!(fs::read(unrelated).unwrap(), b"preserve");
    assert!(runtime_publication_lock_available_for_test(
        &cache, &component
    ));
    cache
        .settle()
        .expect("cancelled producer released owned effects");
}

#[test]
fn missing_runtime_source_cancellation_after_claim_requires_publication_settlement() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime publication executor");
    let join_unknown = std::cell::Cell::new(false);
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        executor.block_on(async {
            let wait = std::time::Duration::from_secs(30);
            let (temporary, session, directory, cache) = runtime_absence_fixture();
            let component = RuntimeId::from("jre-legacy");
            let root = cache.component_root(component.as_str()).unwrap();
            let staging = root.with_file_name("jre-legacy.staging");
            let quarantine = root.with_file_name("jre-legacy.quarantine");
            let source = match tokio::time::timeout(
                wait,
                runtime_source_receipt_fixture(&component, b"committed java"),
            )
            .await
            {
                Ok(source) => source,
                Err(error) => {
                    std::mem::forget((temporary, session, directory, cache));
                    panic!("runtime source acquisition timed out; fixture retained: {error}");
                }
            };
            let unrelated = cache.root().join("unrelated");
            fs::write(&unrelated, b"preserve").unwrap();

            // Three fixed reads consume at most 192 bytes, without file-sized allocation.
            let mut gate = block_runtime_publication_for_test(&root);
            let (cancel, mut control) = runtime_materialization_control();
            let task_cache = cache.clone();
            let mut task = tokio::spawn(async move {
                materialize_missing_runtime_source(
                    &task_cache,
                    &JavaVersion {
                        component: "jre-legacy".into(),
                        major_version: 8,
                    },
                    source,
                    &mut |_| {},
                    &mut control,
                )
                .await
            });
            let reached = tokio::time::timeout(wait, gate.wait_until_reached()).await;
            let staged_bytes = read_fixture_file(&java_executable(&staging));
            let canonical_before = fs::symlink_metadata(&root);
            let held_before = !runtime_publication_lock_available_for_test(&cache, &component);
            let cancellation = cancel.cancel_before_publication();
            gate.release();
            let joined = tokio::time::timeout(wait, &mut task).await;
            let receipt = match joined {
                Ok(Ok(Ok(Some(receipt)))) => receipt,
                Err(error) => {
                    join_unknown.set(true);
                    let detail = format!("gate: {reached:?}, join: {error}");
                    std::mem::forget((temporary, session, directory, cache, task));
                    panic!("publication join unknown; running fixture retained: {detail}");
                }
                Ok(outcome) => {
                    let detail = format!("gate: {reached:?}, producer: {outcome:?}");
                    std::mem::forget((temporary, session, directory, cache, outcome));
                    panic!("publication joined without a receipt; fixture retained: {detail}");
                }
            };

            let receipt_component = receipt.component().clone();
            let receipt_matches_cache = receipt.matches_cache(&cache);
            let receipt_has_quarantine = receipt.quarantine_obligation().is_some();
            let revalidated =
                tokio::time::timeout(wait, receipt.revalidate(&cache, &component)).await;
            let held_by_receipt = !runtime_publication_lock_available_for_test(&cache, &component);
            let canonical_bytes = read_fixture_file(&java_executable(&root));
            let unrelated_bytes = read_fixture_file(&unrelated);
            let staging_after = fs::symlink_metadata(&staging);
            let quarantine_after = fs::symlink_metadata(&quarantine);
            let ready = cache
                .admit_component(component.as_str())
                .map(|component| component.is_some_and(|component| component.contents_verified()));
            drop(receipt);
            let exclusion_released =
                runtime_publication_lock_available_for_test(&cache, &component);
            let settled = cache.settle();
            let revoked = if settled.is_ok() {
                drop((cache, directory));
                let outcome = session.revoke();
                let revoked = matches!(outcome, axial_fs::RootRevokeOutcome::Revoked);
                if !revoked {
                    std::mem::forget((temporary, outcome));
                }
                revoked
            } else {
                std::mem::forget((temporary, session, directory, cache));
                false
            };

            reached.expect("actual publisher reaches the gate after settlement claim");
            let (bytes, length) = staged_bytes.expect("verified Java remains staged at the gate");
            assert_eq!(&bytes[..length], b"committed java");
            assert!(matches!(
                canonical_before,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ));
            assert!(held_before);
            assert_eq!(
                cancellation,
                RuntimeMaterializationCancellation::SettlementRequired
            );
            assert_eq!(receipt_component.as_str(), "jre-legacy");
            assert!(receipt_matches_cache);
            assert!(!receipt_has_quarantine);
            assert!(revalidated.expect("receipt revalidation completes"));
            assert!(held_by_receipt);
            let (bytes, length) = canonical_bytes.expect("published canonical Java is readable");
            assert_eq!(&bytes[..length], b"committed java");
            let (bytes, length) = unrelated_bytes.expect("unrelated file remains readable");
            assert_eq!(&bytes[..length], b"preserve");
            assert!(ready.expect("canonical runtime admission succeeds"));
            assert!(matches!(
                staging_after,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ));
            assert!(matches!(
                quarantine_after,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ));
            assert!(exclusion_released);
            settled.expect("joined publisher released its native effects");
            assert!(revoked, "settled runtime fixture releases its root session");
        });
    }));
    if join_unknown.get() {
        std::mem::forget(executor);
    }
    if let Err(error) = observed {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn missing_runtime_source_preserves_conflicts_after_publication_claim() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime publication executor");
    let join_unknown = std::cell::Cell::new(false);
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        executor.block_on(async {
            let wait = std::time::Duration::from_secs(30);
            // Both fixtures sample at most 384 file bytes and four directory entries.
            for conflict in ["canonical", "quarantine"] {
                let (temporary, session, directory, cache) = runtime_absence_fixture();
                let component = RuntimeId::from("jre-legacy");
                let root = cache.component_root(component.as_str()).unwrap();
                let staging = root.with_file_name("jre-legacy.staging");
                let quarantine = root.with_file_name("jre-legacy.quarantine");
                let source = match tokio::time::timeout(
                    wait,
                    runtime_source_receipt_fixture(&component, b"unpublished java"),
                )
                .await
                {
                    Ok(source) => source,
                    Err(error) => {
                        std::mem::forget((temporary, session, directory, cache));
                        panic!("{conflict} source timed out; fixture retained: {error}");
                    }
                };
                let unrelated = cache.root().join("unrelated");
                fs::write(&unrelated, b"preserve").unwrap();
                let mut gate = block_runtime_publication_for_test(&root);
                let (_cancel, mut control) = runtime_materialization_control();
                let task_cache = cache.clone();
                let mut task = tokio::spawn(async move {
                    let mut ready = false;
                    let result = materialize_missing_runtime_source(
                        &task_cache,
                        &JavaVersion {
                            component: "jre-legacy".into(),
                            major_version: 8,
                        },
                        source,
                        &mut |event| {
                            if matches!(event, RuntimeEnsureEvent::ManagedRuntimeReady { .. }) {
                                ready = true;
                            }
                        },
                        &mut control,
                    )
                    .await;
                    (result, ready)
                });
                let reached = tokio::time::timeout(wait, gate.wait_until_reached()).await;
                let staged_bytes = read_fixture_file(&java_executable(&staging));
                let canonical_before = fs::symlink_metadata(&root);
                let quarantine_before = fs::symlink_metadata(&quarantine);
                let held = !runtime_publication_lock_available_for_test(&cache, &component);
                let (foreign, absent) = if conflict == "canonical" {
                    (&root, &quarantine)
                } else {
                    (&quarantine, &root)
                };
                let sentinel = foreign.join("sentinel");
                let injected = fs::create_dir(foreign)
                    .and_then(|()| fs::write(&sentinel, b"foreign publication conflict"));
                gate.release();
                let joined = tokio::time::timeout(wait, &mut task).await;
                let ready = match joined {
                    Ok(Ok((
                        Err(ManagedRuntimeRebuildError::Preparation(
                            JavaRuntimeLookupError::ManagedMutationRefused,
                        )),
                        ready,
                    ))) => ready,
                    Err(error) => {
                        join_unknown.set(true);
                        let detail = format!(
                            "gate: {reached:?}, injection: {injected:?}, join: {error}"
                        );
                        std::mem::forget((temporary, session, directory, cache, task));
                        panic!("{conflict} join unknown; running fixture retained: {detail}");
                    }
                    Ok(outcome) => {
                        let detail = format!(
                            "gate: {reached:?}, injection: {injected:?}, producer: {outcome:?}"
                        );
                        std::mem::forget((temporary, session, directory, cache, outcome));
                        panic!("{conflict} did not refuse before publication; fixture retained: {detail}");
                    }
                };

                let foreign_bytes = read_fixture_file(&sentinel);
                let unrelated_bytes = read_fixture_file(&unrelated);
                let namespace = fs::read_dir(foreign).and_then(|mut entries| {
                    let first = entries.next().transpose()?.map(|entry| entry.file_name());
                    let second = entries.next().transpose()?.map(|entry| entry.file_name());
                    Ok((first, second))
                });
                let staging_after = fs::symlink_metadata(&staging);
                let absent_after = fs::symlink_metadata(absent);
                let exclusion_released =
                    runtime_publication_lock_available_for_test(&cache, &component);
                let settled = cache.settle();
                let revoked = if settled.is_ok() {
                    drop((cache, directory));
                    let outcome = session.revoke();
                    let revoked = matches!(outcome, axial_fs::RootRevokeOutcome::Revoked);
                    if !revoked {
                        std::mem::forget((temporary, outcome));
                    }
                    revoked
                } else {
                    std::mem::forget((temporary, session, directory, cache));
                    false
                };

                reached.expect("missing runtime reaches publication after settlement claim");
                let (bytes, length) = staged_bytes.expect("verified Java is staged at the gate");
                assert_eq!(&bytes[..length], b"unpublished java", "{conflict}");
                assert!(matches!(
                    canonical_before,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ));
                assert!(matches!(
                    quarantine_before,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ));
                assert!(held, "{conflict}");
                injected.expect("foreign directory is introduced while publication is held");
                assert!(!ready, "{conflict} refusal must not announce readiness");
                let (bytes, length) = foreign_bytes.expect("foreign sentinel remains readable");
                assert_eq!(&bytes[..length], b"foreign publication conflict", "{conflict}");
                assert_eq!(
                    namespace.expect("foreign directory remains readable"),
                    (Some(std::ffi::OsString::from("sentinel")), None),
                    "{conflict} namespace must remain exact"
                );
                let (bytes, length) = unrelated_bytes.expect("unrelated canary remains readable");
                assert_eq!(&bytes[..length], b"preserve", "{conflict}");
                assert!(matches!(
                    staging_after,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ));
                assert!(matches!(
                    absent_after,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ));
                assert!(exclusion_released, "{conflict}");
                settled.expect("refused publication released its native effects");
                assert!(revoked, "{conflict} fixture releases its root session");
            }
        });
    }));
    if join_unknown.get() {
        std::mem::forget(executor);
    }
    if let Err(error) = observed {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
async fn authenticated_empty_runtime_file_stages_and_verifies() {
    for compressed in [false, true] {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let component = RuntimeId::from("jre-legacy");
        let root = cache
            .component_root(component.as_str())
            .expect("runtime root");
        let java_bytes = b"authenticated Java fixture".to_vec();
        let java_server = serve_bodies(&java_bytes, 2).await;
        let java_url = java_server.url("/runtime.bin");
        let empty_server = serve_bodies(b"", 1).await;
        let empty_url = empty_server.url("/runtime.bin");
        let compressed_empty = lzma_compress_bytes(b"");
        assert!(!compressed_empty.is_empty());
        let compressed_server = serve_bodies(&compressed_empty, 2).await;
        let compressed_url = compressed_server.url("/runtime.bin");
        let empty = if compressed {
            downloadable_lzma_manifest_file(
                &empty_url,
                0,
                &sha1_hex(b""),
                &compressed_url,
                compressed_empty.len() as u64,
                &sha1_hex(&compressed_empty),
            )
        } else {
            downloadable_manifest_file(&empty_url, 0, &sha1_hex(b""))
        };
        let expected_download_bytes = java_bytes.len() as u64
            + if compressed {
                compressed_empty.len() as u64
            } else {
                0
            };
        let mut java =
            downloadable_manifest_file(&java_url, java_bytes.len() as u64, &sha1_hex(&java_bytes));
        java.executable = true;
        let empty_path = "lib/security/empty-policy";
        let manifest = ComponentManifest {
            files: HashMap::from([
                (runtime_java_manifest_path(), java),
                (empty_path.into(), empty),
            ]),
        };
        let bytes = serde_json::to_vec(&manifest).expect("runtime manifest");
        let url = serve_body(&bytes).await;
        let source = acquire_runtime_source_for_test(
            component.clone(),
            RuntimeDownloadManifest {
                url,
                sha1: sha1_hex(&bytes),
                size: bytes.len() as u64,
            },
        )
        .await
        .expect("authenticated manifest with empty file");
        let mut events = Vec::new();
        let staged =
            stage_managed_runtime(&cache, &component, source, &mut |event| events.push(event))
                .await
                .expect("authenticated empty runtime file must materialize");
        assert!(!root.exists());
        assert_eq!(
            fs::read(staged.staging_root_for_test().join(empty_path)).unwrap(),
            b""
        );
        assert!(events.iter().any(|event| {
            matches!(event,
                RuntimeEnsureEvent::InstallingManagedRuntimeFiles { current: 2, total: 2, bytes_done, bytes_total, .. }
                if *bytes_done == expected_download_bytes && *bytes_total == expected_download_bytes
            )
        }));
        let verified = publish_staged_managed_runtime_and_finalize(staged)
            .await
            .expect("publish verified empty file")
            .into_verified_runtime(&cache, &component, 8)
            .expect("verified runtime tree");
        drop(verified);
        assert!(admit_runtime_component(&cache, component.as_str()).contents_verified());
        assert_eq!(java_server.finish().len(), 1);
        assert_eq!(compressed_server.finish().len(), usize::from(compressed));
        assert!(
            empty_server.finish().is_empty(),
            "authenticated empty content needs no transfer"
        );

        fs::write(root.join(empty_path), b"unexpected bytes").expect("tamper empty runtime file");
        assert!(!admit_runtime_component(&cache, component.as_str()).contents_verified());
    }
}

#[tokio::test]
async fn authenticated_empty_runtime_file_requires_exact_size_and_empty_digest_before_effects() {
    for (missing_size, empty_compressed) in [(false, false), (true, false), (false, true)] {
        let requests = serve_bodies(b"unused", 1).await;
        let url = requests.url("/runtime.bin");
        let mut empty = downloadable_manifest_file(&url, 0, &sha1_hex(b""));
        let raw = empty.downloads.as_mut().unwrap().raw.as_mut().unwrap();
        if missing_size {
            raw.size = None;
        } else if !empty_compressed {
            raw.sha1 = Some(sha1_hex(b"not empty"));
        }
        if empty_compressed {
            empty.downloads.as_mut().unwrap().lzma = Some(ComponentManifestDownload {
                url: url.clone(),
                sha1: Some(sha1_hex(b"")),
                size: Some(0),
            });
        }
        let manifest = ComponentManifest {
            files: HashMap::from([
                (
                    runtime_java_manifest_path(),
                    downloadable_manifest_file(&url, 6, &sha1_hex(b"unused")),
                ),
                ("lib/security/empty-policy".into(), empty),
            ]),
        };
        assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
    }
}

#[tokio::test]
async fn authenticated_empty_runtime_file_cancelled_after_materialization_cleans_stage() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("runtime root");
    let requests = serve_bodies(b"", 1).await;
    let url = requests.url("/runtime.bin");
    let source = authenticated_runtime_source_from_manifest_for_test(
        component.clone(),
        ComponentManifest {
            files: HashMap::from([(
                runtime_java_manifest_path(),
                downloadable_manifest_file(&url, 0, &sha1_hex(b"")),
            )]),
        },
    )
    .expect("authenticated empty fixture source");
    let (sender, mut cancellation) = crate::cancellation::channel();
    let staging_root = root.with_file_name("jre-legacy.staging");
    let mut observed_materialized_file = false;
    let staged = stage_managed_runtime_until_cancelled(
        &cache,
        &component,
        source,
        &mut |event| {
            if matches!(
                event,
                RuntimeEnsureEvent::InstallingManagedRuntimeFiles { current: 1, .. }
            ) {
                assert_eq!(
                    fs::read(staging_root.join(runtime_java_manifest_path())).unwrap(),
                    b""
                );
                observed_materialized_file = true;
                sender.cancel();
            }
        },
        &mut cancellation,
    )
    .await
    .expect("cancelled staging settles");
    assert!(observed_materialized_file);
    assert!(staged.is_none());
    assert!(!root.exists());
    assert!(!staging_root.exists());
    assert!(requests.finish().is_empty());
}

#[tokio::test]
async fn cached_runtime_verification_holds_exact_publication_locks_until_consumed() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let staged = stage_java(&cache, &component, b"cached java").await;
    let source = publish_staged_managed_runtime_and_finalize(staged)
        .await
        .expect("managed runtime publish")
        .into_verified_runtime(&cache, &component, 8)
        .expect("verified managed runtime")
        .into_parts()
        .1;
    let (_cancellation_sender, mut cancellation) = crate::cancellation::channel();
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
    assert!(
        !runtime_publication_lock_available_for_test(&cache, &component),
        "cached verification result must retain the exact publication lease"
    );
    let (_runtime, _source) = verified.into_parts();
    assert!(
        runtime_publication_lock_available_for_test(&cache, &component),
        "verified result consumption must release the exact publication lease"
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
    let java_relative_path = runtime_java_manifest_path();
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
    let (cancellation_tx, mut cancellation) = crate::cancellation::channel();
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

    tokio::time::timeout(std::time::Duration::from_secs(60), request_started_rx)
        .await
        .expect("runtime download should begin")
        .expect("runtime request signal");
    assert!(staging_root.is_dir());
    cancellation_tx.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), stage_task)
        .await
        .expect("cancelled runtime stage should drain")
        .expect("runtime stage task")
        .expect("runtime stage cancellation should clean up");

    assert!(result.is_none());
    assert!(!staging_root.exists());
    assert!(!root.exists());
    tokio::time::timeout(std::time::Duration::from_secs(60), connection_closed_rx)
        .await
        .expect("cancelled runtime request should close")
        .expect("runtime connection close signal");
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
    let compressed_url = serve_body(&compressed_bytes).await;
    let java_relative_path = runtime_java_manifest_path();
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
    let contender_source = runtime_source_receipt_fixture(&component, b"contender java").await;
    let gate = block_runtime_decompression_for_test(java_executable(&staging_root));
    let (cancellation_tx, mut cancellation) = crate::cancellation::channel();
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

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
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
    assert!(
        !runtime_publication_lock_available_for_test(&cache, &component),
        "blocked decompression must retain the publication lease"
    );
    assert_eq!(
        fs::read(root.join("sentinel")).expect("canonical sentinel while cancelled"),
        b"canonical"
    );

    gate.release
        .send(())
        .expect("release runtime decompression worker");
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), stage_task)
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
    assert!(
        runtime_publication_lock_available_for_test(&cache, &component),
        "cancelled stage must clean up before releasing the publication lease"
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
    let contender = tokio::time::timeout(std::time::Duration::from_secs(60), contender_task)
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
async fn runtime_source_receipt_verifies_integrity_before_parse() {
    let bytes = b"not json";
    let zero = "0000000000000000000000000000000000000000".to_string();
    for (sha1, size, kind, detail) in [
        (
            sha1_hex(bytes),
            bytes.len() as u64 + 1,
            RuntimeSourceFailureKind::IntegrityMismatch,
            "runtime component manifest size mismatch",
        ),
        (
            zero,
            bytes.len() as u64,
            RuntimeSourceFailureKind::IntegrityMismatch,
            "runtime component manifest checksum mismatch",
        ),
        (
            sha1_hex(bytes),
            bytes.len() as u64,
            RuntimeSourceFailureKind::MetadataInvalid,
            "expected ident",
        ),
    ] {
        let url = serve_body(bytes).await;
        let error = acquire_runtime_source_for_test(
            RuntimeId::from("java-runtime-delta"),
            RuntimeDownloadManifest { url, sha1, size },
        )
        .await
        .expect_err("unverified or malformed runtime source must fail");
        let failure = expect_runtime_source_failure(error, kind);
        assert!(failure.detail().contains(detail), "{failure:?}");
    }
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
fn managed_runtime_components_stay_bound_to_their_cache() {
    let first = ManagedRuntimeCache::isolated_for_test().expect("first runtime cache");
    let second = ManagedRuntimeCache::isolated_for_test().expect("second runtime cache");
    let first_root = first
        .component_root("java-runtime-delta")
        .expect("first component root");
    let second_root = second
        .component_root("java-runtime-delta")
        .expect("second component root");
    fs::create_dir(&first_root).expect("first runtime component");
    fs::create_dir(&second_root).expect("second runtime component");
    let first_component = admit_runtime_component(&first, "java-runtime-delta");
    let second_component = admit_runtime_component(&second, "java-runtime-delta");

    assert_ne!(first.root(), second.root());
    assert!(first_component.belongs_to(&first));
    assert!(!first_component.belongs_to(&second));
    assert!(second_component.belongs_to(&second));
    assert!(!second_component.belongs_to(&first));
}

#[test]
fn structural_runtime_discovery_does_not_parse_incomplete_manifest_proof() {
    for (case, proof) in [
        ("empty manifest", &br#"{"files":{}}"#[..]),
        (
            "missing raw download",
            br#"{"files":{"bin/java":{"type":"file","downloads":{}}}}"#,
        ),
    ] {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let root = cache
            .component_root("java-runtime-delta")
            .expect("runtime root");
        write_runtime_executable_fixture(&root);
        fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
        fs::write(root.join(".axial-runtime-manifest.json"), proof)
            .expect("incomplete runtime manifest proof");

        assert!(
            !admit_runtime_component(&cache, "java-runtime-delta").contents_verified(),
            "{case}"
        );
    }
}

#[test]
fn explicit_full_runtime_verifier_detects_same_root_content_drift() {
    let (cache, root) = ready_runtime();
    assert!(admit_runtime_component(&cache, "java-runtime-delta").contents_verified());

    fs::write(java_executable(&root), b"changed java").expect("modify java");
    make_executable(&java_executable(&root));

    assert!(!admit_runtime_component(&cache, "java-runtime-delta").contents_verified());
}

#[cfg(unix)]
#[test]
fn explicit_full_runtime_verifier_detects_manifest_link_drift() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let root = cache
        .component_root("java-runtime-delta")
        .expect("runtime root");
    write_runtime_executable_fixture(&root);
    let link = java_executable(&root).with_file_name("java-link");
    std::os::unix::fs::symlink("java", &link).expect("runtime symlink");
    write_runtime_manifest_proof_for_java_and_link(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");

    assert!(admit_runtime_component(&cache, "java-runtime-delta").contents_verified());

    fs::remove_file(link).expect("remove runtime symlink");
    assert!(!admit_runtime_component(&cache, "java-runtime-delta").contents_verified());
}

#[test]
fn managed_runtime_launch_receipt_rejects_program_and_executable_replacement() {
    let _serial = runtime_launch_receipt_test_guard();
    let (cache, _root) = ready_runtime();
    let component = admit_runtime_component(&cache, "java-runtime-delta");
    let java = component.java_executable_path();
    let receipt = component.launch_receipt().expect("launch receipt");

    receipt
        .validate_program(&java)
        .expect("matching retained executable");
    assert!(
        receipt
            .validate_program(&java.with_file_name("java-other"))
            .is_err()
    );

    let displaced = java.with_file_name("java-displaced");
    fs::rename(&java, &displaced).expect("displace Java executable");
    fs::write(&java, b"replacement java").expect("replacement Java executable");
    make_executable(&java);
    assert!(receipt.validate_program(&java).is_err());
}

#[cfg(unix)]
#[test]
fn managed_runtime_launch_receipt_rejects_component_root_replacement() {
    let _serial = runtime_launch_receipt_test_guard();
    let (cache, root) = ready_runtime();
    let receipt = admit_runtime_component(&cache, "java-runtime-delta")
        .launch_receipt()
        .expect("launch receipt");

    let displaced = root.with_file_name("java-runtime-delta-displaced");
    fs::rename(&root, &displaced).expect("displace component root");
    write_runtime_executable_fixture(&root);
    write_runtime_manifest_proof_for_java(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("replacement marker");

    assert!(receipt.validate_program(&java_executable(&root)).is_err());
}

#[cfg(unix)]
#[test]
fn managed_runtime_launch_receipt_retains_owned_java_link_target() {
    let _serial = runtime_launch_receipt_test_guard();
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let root = cache
        .component_root("java-runtime-delta")
        .expect("runtime root");
    let java = java_executable(&root);
    let java_parent = java.parent().expect("java parent");
    fs::create_dir_all(java_parent).expect("java parent");
    let target = java.with_file_name("java-real");
    fs::write(&target, b"java").expect("Java link target");
    make_executable(&target);
    std::os::unix::fs::symlink("java-real", &java).expect("platform Java link");
    write_runtime_manifest_proof_for_platform_java_link(&root, "java-real");
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    let component = admit_runtime_component(&cache, "java-runtime-delta");
    assert!(component.structurally_ready());
    let receipt = component.launch_receipt().expect("linked launch receipt");
    receipt
        .validate_program(&java)
        .expect("linked Java is launchable");

    let displaced = target.with_file_name("java-real-displaced");
    fs::rename(&target, &displaced).expect("displace Java link target");
    fs::write(&target, b"replacement java").expect("replacement Java link target");
    make_executable(&target);
    assert!(receipt.validate_program(&java).is_err());

    let receipt = component
        .launch_receipt()
        .expect("replacement linked launch receipt");
    let alternate = java.with_file_name("java-alternate");
    fs::write(&alternate, b"java").expect("alternate Java target");
    make_executable(&alternate);
    fs::remove_file(&java).expect("remove Java link");
    std::os::unix::fs::symlink("java-alternate", &java).expect("replace Java link");
    assert!(receipt.validate_program(&java).is_err());
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
fn runtime_manifest_selection_prefers_native_then_fallback_platform() {
    let entry = |platform: &str| {
        serde_json::json!([{ "manifest": {
            "url": format!("https://example.invalid/{platform}/jre-legacy.json"),
            "sha1": "0000000000000000000000000000000000000000",
            "size": 1
        } }])
    };
    let none = serde_json::json!([]);
    for (native, fallback, expected) in [
        (entry("mac-os-arm64"), entry("mac-os"), Some("mac-os-arm64")),
        (none.clone(), entry("mac-os"), Some("mac-os")),
        (none.clone(), none.clone(), None),
    ] {
        let manifest = runtime_manifest_fixture(serde_json::json!({
            "mac-os-arm64": { "jre-legacy": native },
            "mac-os": { "jre-legacy": fallback },
        }));
        let selected =
            select_runtime_manifest(&manifest, &RuntimeId::from("jre-legacy"), "mac-os-arm64");
        match (selected, expected) {
            (Ok(descriptor), Some(platform)) => assert_eq!(
                descriptor.url,
                format!("https://example.invalid/{platform}/jre-legacy.json")
            ),
            (Err(error), None) => assert!(matches!(
                error,
                JavaRuntimeLookupError::UnsupportedPlatform { component, platform }
                    if component == "jre-legacy" && platform == "mac-os-arm64"
            )),
            (selected, expected) => panic!("{expected:?}: {selected:?}"),
        }
    }
}

#[test]
fn mach_o_sniff_classifies_arm64_compatibility() {
    let thin = |cpu: u32| {
        let mut bytes = Vec::new();
        bytes.extend(0xfeed_facfu32.to_le_bytes());
        bytes.extend(cpu.to_le_bytes());
        bytes.extend([0u8; 24]);
        bytes
    };
    let fat = |magic: u32, arches: &[&[u8]]| {
        let mut bytes = Vec::new();
        bytes.extend(magic.to_be_bytes());
        bytes.extend(u32::try_from(arches.len()).unwrap().to_be_bytes());
        for arch in arches {
            bytes.extend_from_slice(arch);
        }
        bytes
    };
    for (case, bytes, expected) in [
        (
            "thin arm64",
            thin(0x0100_000c),
            MachOArm64Compatibility::HasArm64Slice,
        ),
        (
            "thin x86_64",
            thin(0x0100_0007),
            MachOArm64Compatibility::LacksArm64Slice,
        ),
        (
            "fat with arm64",
            fat(
                0xcafe_babe,
                &[&fat_arch32_be(0x0100_0007), &fat_arch32_be(0x0100_000c)],
            ),
            MachOArm64Compatibility::HasArm64Slice,
        ),
        (
            "fat without arm64",
            fat(0xcafe_babe, &[&fat_arch32_be(0x0100_0007)]),
            MachOArm64Compatibility::LacksArm64Slice,
        ),
        (
            "fat64 with arm64",
            fat(
                0xcafe_babf,
                &[&fat_arch64_be(0x0100_0007), &fat_arch64_be(0x0100_000c)],
            ),
            MachOArm64Compatibility::HasArm64Slice,
        ),
        (
            "garbage",
            b"not a mach-o".to_vec(),
            MachOArm64Compatibility::UnknownCompatible,
        ),
    ] {
        assert_eq!(parse_mach_o_arm64_compatibility(&bytes), expected, "{case}");
    }
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
    let java_url = serve_body(&java_bytes).await;
    let cfg_url = serve_body(&cfg_bytes).await;
    let mut files = HashMap::new();
    let java_relative_path = runtime_java_manifest_path();
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
    let component_manifest_url = serve_body(&component_manifest_bytes).await;
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

    assert!(root.join(".axial-runtime-manifest.json").is_file());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEnsureEvent::ManagedRuntimeReady { .. }))
    );
}

#[tokio::test]
async fn behavior_contract_runtime_failure_receipt_restores_the_canonical_runtime() {
    let (cache, component, root) = seeded_runtime();
    let staged = stage_java(&cache, &component, b"replacement java").await;
    let staging_root = staged.staging_root_for_test().to_path_buf();

    let error = publish_staged_managed_runtime_with_promotion_failure_for_test(staged)
        .await
        .expect_err("injected promotion failure");

    let ManagedRuntimeRebuildError::Effect(effect) = error else {
        panic!("canonical displacement must return sealed effect evidence");
    };
    assert_eq!(effect.component(), &component);
    assert!(effect.matches_cache(&cache));
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
    let (cache, component, root) = seeded_runtime();
    let staged = stage_java(&cache, &component, b"replacement java").await;
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
    let (cache, component, root) = seeded_runtime();
    let staged = stage_java(&cache, &component, b"replacement java").await;
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
    let (cache, component, root) = seeded_runtime();
    let staged = stage_java(&cache, &component, b"replacement java").await;

    let receipt = publish_staged_managed_runtime_and_finalize(staged)
        .await
        .expect("ordinary managed runtime publish");

    assert!(receipt.quarantine_obligation().is_none());
    assert!(receipt.revalidate(&cache, &component).await);
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
}

#[tokio::test]
async fn ordinary_quarantine_finalization_failure_retains_effect_evidence() {
    let (cache, component, root) = seeded_runtime();
    let staged = stage_java(&cache, &component, b"replacement java").await;

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
    assert!(admit_runtime_component(&cache, component.as_str()).contents_verified());
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
async fn managed_runtime_abandoned_stage_uses_one_recoverable_fixed_slot() {
    let (cache, component, root) = seeded_runtime();

    let abandoned = stage_java(&cache, &component, b"abandoned java").await;
    let staging_root = abandoned.staging_root_for_test().to_path_buf();
    drop(abandoned);
    assert!(staging_root.exists());
    assert_eq!(
        fs::read(java_executable(&staging_root)).expect("abandoned staged java"),
        b"abandoned java"
    );

    let replacement = stage_java(&cache, &component, b"replacement java").await;
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
    let (cache, component, root) = seeded_runtime();

    let first_stage = stage_java(&cache, &component, b"first java").await;
    let first_commit = publish_staged_managed_runtime(first_stage)
        .await
        .expect("first retained managed runtime publish");
    let quarantine = first_commit
        .quarantine_root_for_test()
        .expect("retained quarantine");
    drop(first_commit);

    let second_stage = stage_java(&cache, &component, b"second java").await;
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
    let (cache, component, root) = seeded_runtime();

    let first_stage = stage_java(&cache, &component, b"first java").await;
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
    let second_stage = stage_java(&cache, &component, b"second java").await;
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
    let first_stage = stage_java(&cache, &component, b"first java").await;
    let first_receipt = publish_staged_managed_runtime(first_stage)
        .await
        .expect("first managed runtime publish");
    let second_source = runtime_source_receipt_fixture(&component, b"second java").await;
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

    let second_stage = tokio::time::timeout(std::time::Duration::from_secs(60), second_stage_task)
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
async fn managed_runtime_admission_rejects_unsafe_manifests_before_effects() {
    type Entries = Vec<(String, ComponentManifestFile)>;
    fn file(url: &str) -> ComponentManifestFile {
        downloadable_manifest_file(url, 1, "0000000000000000000000000000000000000000")
    }
    fn entries(entries: Vec<(&str, ComponentManifestFile)>) -> Entries {
        entries
            .into_iter()
            .map(|(path, file)| (path.to_string(), file))
            .collect()
    }
    let cases: [(&str, fn(&str) -> Entries); 10] = [
        ("oversized file", |url| {
            let mut java = file(url);
            java.downloads.as_mut().unwrap().raw.as_mut().unwrap().size = Some((128 << 20) + 1);
            vec![(runtime_java_manifest_path(), java)]
        }),
        ("missing size", |url| {
            let mut java = file(url);
            java.downloads.as_mut().unwrap().raw.as_mut().unwrap().size = None;
            vec![(runtime_java_manifest_path(), java)]
        }),
        ("over-entry tree", |url| {
            let mut tree = (0..4096)
                .map(|index| (format!("entry-{index}"), manifest_file("directory")))
                .collect::<Entries>();
            tree.push((runtime_java_manifest_path(), file(url)));
            tree
        }),
        ("case aliases", |url| {
            entries(vec![
                ("bin/java.exe", file(url)),
                ("BIN/JAVA.EXE", file(url)),
            ])
        }),
        ("explicit folded prefix alias", |url| {
            entries(vec![
                ("bin", manifest_file("directory")),
                ("BIN/java", file(url)),
            ])
        }),
        ("implicit folded prefix alias", |url| {
            entries(vec![("bin/a", file(url)), ("BIN/b", file(url))])
        }),
        ("case-mismatched link target", |url| {
            entries(vec![
                ("legal/base/LICENSE", file(url)),
                ("legal/module/LICENSE", manifest_link("../BASE/LICENSE")),
            ])
        }),
        ("dangling link target", |url| {
            entries(vec![
                ("bin/java", file(url)),
                ("legal/module/LICENSE", manifest_link("../base/LICENSE")),
            ])
        }),
        ("backslash link target", |url| {
            entries(vec![
                ("legal/base/LICENSE", file(url)),
                ("legal/module/LICENSE", manifest_link(r"..\base\LICENSE")),
            ])
        }),
        ("link loop", |url| {
            entries(vec![
                ("bin/java", file(url)),
                ("legal/loop", manifest_link("loop")),
            ])
        }),
    ];
    for (case, files) in cases {
        eprintln!("runtime admission case: {case}");
        let requests = serve_bodies(b"unused", 1).await;
        let manifest = ComponentManifest {
            files: files(&requests.url("/runtime.bin")).into_iter().collect(),
        };
        assert_managed_manifest_rejection_preserves_state(manifest, requests).await;
    }
}

#[tokio::test]
async fn managed_runtime_stage_rejects_extra_tree_entries_before_displacement() {
    let (cache, component, root) = seeded_runtime();
    let staged = stage_java(&cache, &component, b"replacement java").await;
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
    let staged = stage_java(&cache, &component, b"authenticated java").await;
    let receipt = publish_staged_managed_runtime(staged)
        .await
        .expect("managed runtime publish");
    assert!(receipt.revalidate(&cache, &component).await);

    fs::write(root.join(".axial-ready"), b"nope").expect("tamper ready marker");

    assert!(!receipt.revalidate(&cache, &component).await);
}

/// An isolated cache whose canonical `jre-legacy` root holds an `original` sentinel.
fn seeded_runtime() -> (ManagedRuntimeCache, RuntimeId, PathBuf) {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let component = RuntimeId::from("jre-legacy");
    let root = cache
        .component_root(component.as_str())
        .expect("managed runtime component root");
    fs::create_dir(&root).expect("canonical runtime root");
    fs::write(root.join("sentinel"), b"original").expect("canonical sentinel");
    (cache, component, root)
}

async fn stage_java(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    java: &[u8],
) -> super::install::StagedManagedRuntime {
    let source = runtime_source_receipt_fixture(component, java).await;
    stage_managed_runtime(cache, component, source, &mut |_| {})
        .await
        .expect("managed runtime stage")
}

async fn runtime_source_receipt_fixture(
    component: &RuntimeId,
    java_bytes: &[u8],
) -> RuntimeSourceReceipt {
    let java_url = serve_body(java_bytes).await;
    let java_relative_path = runtime_java_manifest_path();
    let mut java_file =
        downloadable_manifest_file(&java_url, java_bytes.len() as u64, &sha1_hex(java_bytes));
    java_file.executable = true;
    let component_manifest = ComponentManifest {
        files: HashMap::from([(java_relative_path, java_file)]),
    };
    let manifest_bytes =
        serde_json::to_vec(&component_manifest).expect("component manifest fixture");
    let manifest_url = serve_body(&manifest_bytes).await;
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
    requests: Server,
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
    assert!(requests.finish().is_empty());
    assert!(events.is_empty());
    assert_eq!(
        fs::read(root.join("sentinel")).expect("unchanged canonical sentinel"),
        b"canonical"
    );
    assert_eq!(
        fs::read(staging_root.join("sentinel")).expect("unchanged staging sentinel"),
        b"staging"
    );
    assert!(!root.with_file_name("jre-legacy.quarantine").exists());
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

#[test]
fn runtime_download_verification_compares_only_declared_metadata() {
    const ACTUAL_SHA1: &str = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";
    for (case, size, sha1, outcome) in [
        (
            "matching metadata",
            Some(5),
            Some("AAF4C61DDCC5E8A2DABEDE0F3B482CD9AEA9434D"),
            Ok(()),
        ),
        (
            "size mismatch",
            Some(6),
            None,
            Err(RuntimeDownloadIntegrityError::SizeMismatch {
                file: "bin/java".to_string(),
                expected: 6,
                actual: 5,
            }),
        ),
        (
            "sha1 mismatch",
            None,
            Some("0000000000000000000000000000000000000000"),
            Err(RuntimeDownloadIntegrityError::Sha1Mismatch {
                file: "bin/java".to_string(),
                expected: "0000000000000000000000000000000000000000".to_string(),
                actual: ACTUAL_SHA1.to_string(),
            }),
        ),
        ("missing metadata", None, None, Ok(())),
    ] {
        let result =
            verify_runtime_download("bin/java", &expected(size, sha1), &actual(5, ACTUAL_SHA1));

        assert_eq!(result, outcome, "{case}");
    }
}

#[tokio::test]
async fn runtime_manifest_file_streams_and_imports_verified_source() {
    let root = unique_temp_root("axial-runtime-download-stream-test");
    let url = serve_body(b"hello").await;
    let file = downloadable_manifest_file(&url, 5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

    install_runtime_manifest_file(&test_runtime_component(), &root, "bin/java", file)
        .await
        .expect("runtime download");

    assert_eq!(
        fs::read(root.join("bin/java")).expect("downloaded file"),
        b"hello"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn ephemeral_processor_manifest_admission_enforces_paths_topology_and_bounds() {
    let zero = "0000000000000000000000000000000000000000";
    let file = downloadable_manifest_file("https://example.invalid/java", 1, zero);
    let manifest = |entries: Vec<(&str, ComponentManifestFile)>| ComponentManifest {
        files: entries
            .into_iter()
            .map(|(path, file)| (path.to_string(), file))
            .collect(),
    };
    validate_ephemeral_processor_manifest_for_test(
        &manifest(vec![
            ("legal/base/LICENSE", file.clone()),
            ("legal/module/LICENSE", manifest_link("../base/LICENSE")),
            ("legal/module/base", manifest_link("../base")),
        ]),
        1,
    )
    .expect("declared and implicit portable link targets are admitted");

    let mut over_entries = (0..4091)
        .map(|index| (format!("entry-{index}"), manifest_file("directory")))
        .collect::<HashMap<_, _>>();
    over_entries.insert(
        "bin/java".to_string(),
        downloadable_lzma_manifest_file(
            "https://example.invalid/java",
            1,
            zero,
            "https://example.invalid/java.lzma",
            1,
            zero,
        ),
    );
    let rejected = [
        (
            "case alias",
            manifest(vec![("bin/java", file.clone()), ("BIN/JAVA", file.clone())]),
            1,
        ),
        (
            "runtime-owned scratch name",
            manifest(vec![
                ("bin/java", file.clone()),
                ("bin/java.axial-tmp", file.clone()),
            ]),
            1,
        ),
        (
            "ready marker alias",
            manifest(vec![(".AXIAL-READY", file.clone())]),
            1,
        ),
        (
            "directory link ancestor",
            manifest(vec![
                ("a", manifest_file("directory")),
                ("a/link", manifest_link("../a")),
            ]),
            1,
        ),
        (
            "oversized file",
            manifest(vec![(
                "bin/java",
                downloadable_manifest_file("https://example.invalid/java", (128 << 20) + 1, zero),
            )]),
            1,
        ),
        (
            "checked byte overflow",
            manifest(vec![("bin/java", file.clone())]),
            u64::MAX,
        ),
        (
            "lzma peak entry",
            ComponentManifest {
                files: over_entries,
            },
            1,
        ),
    ];
    for (case, manifest, manifest_bytes) in rejected {
        assert!(
            matches!(
                validate_ephemeral_processor_manifest_for_test(&manifest, manifest_bytes),
                Err(JavaRuntimeLookupError::RuntimeSource(failure))
                    if failure.kind() == RuntimeSourceFailureKind::PolicyRejected
            ),
            "{case}"
        );
    }
}

#[tokio::test]
async fn runtime_manifest_file_retries_transient_provider_errors() {
    let root = unique_temp_root("axial-runtime-download-retry-test");
    let attempts = serve(vec![
        response("503 Service Unavailable", &[], b"try again"),
        response("503 Service Unavailable", &[], b"try again"),
        response("200 OK", &[], b"hello"),
    ])
    .await;
    let url = attempts.url("/runtime.bin");
    let file = downloadable_manifest_file(&url, 5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

    install_runtime_manifest_file(&test_runtime_component(), &root, "bin/java", file)
        .await
        .expect("runtime download should retry transient failures");

    assert_eq!(attempts.finish().len(), 3);
    assert_eq!(
        fs::read(root.join("bin/java")).expect("retried runtime file"),
        b"hello"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_manifest_file_discards_transient_source_on_verification_error() {
    let root = unique_temp_root("axial-runtime-download-cleanup-test");
    let url = serve_body(b"hello").await;
    let file = downloadable_manifest_file(&url, 6, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

    let result =
        install_runtime_manifest_file(&test_runtime_component(), &root, "bin/java", file).await;

    assert!(matches!(
        &result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::IntegrityMismatch
    ));
    assert!(!root.join("bin/java").exists());
    assert!(
        fs::read_dir(&root)
            .expect("managed runtime test root")
            .all(|entry| !entry
                .expect("managed runtime root entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".axial-stage-"))
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_file_download_rejects_oversized_content_length() {
    let root = unique_temp_root("axial-runtime-download-content-length-test");
    let url = serve(vec![response(
        "200 OK",
        &[("Content-Length", "6")],
        b"hello",
    )])
    .await
    .url("/runtime.bin");
    let file = downloadable_manifest_file(&url, 5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

    let result =
        install_runtime_manifest_file(&test_runtime_component(), &root, "bin/java", file).await;

    assert!(matches!(
        &result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::IntegrityMismatch
                && failure.detail().contains("ContentLengthContractMismatch")
    ));
    assert!(!root.join("bin/java").exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_manifest_file_requires_valid_checksum_proof() {
    for (case, sha1) in [
        ("missing", None),
        ("invalid", Some("not-a-sha1".to_string())),
    ] {
        let root = unique_temp_root("axial-runtime-checksum-proof-test");
        let file = ComponentManifestFile {
            kind: "file".to_string(),
            executable: false,
            downloads: Some(ComponentManifestDownloads {
                raw: Some(ComponentManifestDownload {
                    url: "https://example.test/runtime.bin".to_string(),
                    sha1,
                    size: Some(4),
                }),
                lzma: None,
            }),
            target: None,
        };

        let result =
            install_runtime_manifest_file(&test_runtime_component(), &root, "bin/java", file).await;

        assert!(
            matches!(
                result,
                Err(JavaRuntimeLookupError::RuntimeSource(failure))
                    if failure.component() == &test_runtime_component()
                        && failure.kind() == RuntimeSourceFailureKind::MetadataInvalid
                        && failure.detail().contains("missing checksum proof")
            ),
            "{case}"
        );
        assert!(!root.join("bin").join("java").exists(), "{case}");
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn runtime_manifest_file_prefers_lzma_and_verifies_decompressed_output() {
    let root = unique_temp_root("axial-runtime-lzma-test");
    let raw_bytes = b"decompressed runtime file".to_vec();
    let compressed_bytes = lzma_compress_bytes(&raw_bytes);
    let lzma_url = serve_body(&compressed_bytes).await;
    let file = downloadable_lzma_manifest_file(
        "http://127.0.0.1:9/raw-runtime-file",
        raw_bytes.len() as u64,
        &sha1_hex(&raw_bytes),
        &lzma_url,
        compressed_bytes.len() as u64,
        &sha1_hex(&compressed_bytes),
    );

    install_runtime_manifest_file(&test_runtime_component(), &root, "bin/java", file)
        .await
        .expect("runtime lzma file install");

    assert_eq!(
        fs::read(root.join("bin").join("java")).expect("decompressed runtime file"),
        raw_bytes
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_file_download_rejects_stream_past_expected_size_and_removes_temp() {
    let root = unique_temp_root("axial-runtime-download-stream-bound-test");
    let url = serve(vec![
        b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello!".to_vec(),
    ])
    .await
    .url("/runtime.bin");
    let file = downloadable_manifest_file(&url, 5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

    let result =
        install_runtime_manifest_file(&test_runtime_component(), &root, "bin/java", file).await;

    assert!(matches!(
        &result,
        Err(JavaRuntimeLookupError::RuntimeSource(failure))
            if failure.component() == &test_runtime_component()
                && failure.kind() == RuntimeSourceFailureKind::IntegrityMismatch
                && failure.detail().contains("ByteLimitExceeded")
    ));
    assert!(!root.join("bin/java").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_install_futures_stay_small_enough_for_tokio_workers() {
    let root = Path::new("/tmp/axial-runtime-future-size");
    let file = downloadable_manifest_file(
        "https://example.test/runtime.bin",
        8,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    let spawned_root = root.to_path_buf();
    let spawned_file = file.clone();
    let spawned_future = async move {
        Box::pin(install_runtime_manifest_file(
            &test_runtime_component(),
            &spawned_root,
            "bin/java",
            spawned_file,
        ))
        .await
    };

    let file_bytes = std::mem::size_of_val(&install_runtime_manifest_file(
        &test_runtime_component(),
        root,
        "bin/java",
        file.clone(),
    ));
    assert!(
        file_bytes < 4096,
        "runtime manifest file install future should stay small: {file_bytes} bytes"
    );
    let spawned_bytes = std::mem::size_of_val(&spawned_future);
    assert!(
        spawned_bytes < 4096,
        "spawned runtime manifest file install future should stay small: {spawned_bytes} bytes"
    );
}

fn unique_temp_root(label: &str) -> PathBuf {
    crate::test_temp_root().join(format!(
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

/// A verified `java-runtime-delta` with a Java executable, manifest proof and ready marker.
fn ready_runtime() -> (ManagedRuntimeCache, PathBuf) {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let root = cache
        .component_root("java-runtime-delta")
        .expect("runtime root");
    write_runtime_executable_fixture(&root);
    write_runtime_manifest_proof_for_java(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    (cache, root)
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
    let manifest = serde_json::from_value::<ComponentManifest>(serde_json::json!({
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
    }))
    .expect("runtime manifest");
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        component_manifest_proof_bytes(&manifest).expect("canonical manifest"),
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
    let manifest = serde_json::from_value::<ComponentManifest>(serde_json::json!({
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
    }))
    .expect("runtime manifest");
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        component_manifest_proof_bytes(&manifest).expect("canonical manifest"),
    )
    .expect("write runtime manifest proof with link");
}

#[cfg(unix)]
fn write_runtime_manifest_proof_for_platform_java_link(root: &Path, target_name: &str) {
    let java = java_executable(root);
    let target = java.with_file_name(target_name);
    let bytes = fs::read(&target).expect("read Java link target");
    let java_relative = java
        .strip_prefix(root)
        .expect("Java link under root")
        .to_string_lossy()
        .replace('\\', "/");
    let target_relative = target
        .strip_prefix(root)
        .expect("Java target under root")
        .to_string_lossy()
        .replace('\\', "/");
    let manifest = serde_json::from_value::<ComponentManifest>(serde_json::json!({
        "files": {
            java_relative: {
                "type": "link",
                "target": target_name
            },
            target_relative: {
                "type": "file",
                "executable": true,
                "downloads": {
                    "raw": {
                        "url": "https://example.invalid/java",
                        "sha1": sha1_hex(&bytes),
                        "size": bytes.len()
                    }
                }
            }
        }
    }))
    .expect("linked runtime manifest");
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        component_manifest_proof_bytes(&manifest).expect("canonical linked manifest"),
    )
    .expect("write linked runtime manifest proof");
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

/// Serves `body` once; the detached server outlives the returned URL.
async fn serve_body(body: &[u8]) -> String {
    serve(vec![response("200 OK", &[], body)])
        .await
        .url("/runtime.bin")
}

async fn serve_bodies(body: &[u8], count: usize) -> Server {
    serve(vec![response("200 OK", &[], body); count]).await
}
