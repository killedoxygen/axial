use super::assets::{
    copy_virtual_asset_if_missing, copy_virtual_assets, repair_virtual_assets_from_index,
    unique_asset_object_jobs, virtual_asset_destination,
};
use super::client::{adaptive_download_concurrency, build_http_client};
use super::facts::execution_download_fact;
use super::install::{
    observe_managed_install_lease_wait_for_test,
    roll_back_managed_install_component_after_first_row_for_test,
};
use super::integrity::hash_file;
use super::libraries::{library_jobs_for, library_verification_plans_for};
use super::path_safety::{safe_download_target_label, windows_verbatim_path_string};
use super::promotion::sweep_stale_promotion_backups;
use super::runtime::{
    runtime_ensure_progress, runtime_pipeline_for_test, settle_runtime_pipeline,
    settle_runtime_pipeline_after_failure,
};
use super::transfer::{
    acquire_authenticated_selected_artifact_source_with_retry_delays_for_test,
    promote_launcher_managed_artifact_temp_once, remove_stale_download_temp,
};
use super::*;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRoot, MAX_TIER2_AGGREGATE_BYTES,
    MAX_TIER2_ARTIFACT_BYTES,
};
use crate::launch::{JavaVersion, Library, LibraryArtifact, LibraryDownload, maven_to_path};
use crate::managed_blocking::{ManagedBlockingCheckpoint, ManagedBlockingWorkers};
use crate::managed_fs::ManagedDir;
use crate::managed_publication::ManagedRootPublicationLease;
use crate::manifest::VersionManifest;
use crate::paths::{assets_dir, versions_dir};
use crate::rules::Environment;
use crate::runtime::{
    RuntimeEnsureEvent, RuntimeId, RuntimeSourceReceipt, TestRuntimeSourceDescriptor,
    authenticated_test_runtime_source,
};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn install_version_emits_terminal_error_when_setup_fails() {
    let root = temp_dir("setup-failure");
    fs::create_dir_all(&root).expect("create root");
    fs::write(versions_dir(&root), b"not a directory").expect("write versions sentinel");

    let downloader = test_manifest_downloader(
        &root,
        "1.20.1",
        "https://example.invalid/1.20.1.json",
        "abcdef1234567890abcdef1234567890abcdef12",
    );
    let mut events = Vec::new();
    let result = downloader
        .install_version("1.20.1", |progress| events.push(progress))
        .await;

    assert!(result.is_err());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].phase, "version_json");
    assert_eq!((events[0].current, events[0].total), (0, 1));
    let event = &events[1];
    assert_eq!(event.phase, "error");
    assert_eq!(event.current, 0);
    assert_eq!(event.total, 0);
    assert_eq!(event.file, None);
    assert!(event.error.is_some());
    assert!(event.done);

    let _ = fs::remove_file(root.join("versions"));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn invalid_library_preflight_precedes_asset_runtime_and_client_effects() {
    let root = temp_dir("library-preflight-before-effects");
    let client_path = versions_dir(&root).join("preflight").join("preflight.jar");
    let asset_index_path = assets_dir(&root)
        .join("indexes")
        .join("preflight-assets.json");
    for path in [&client_path, &asset_index_path] {
        fs::create_dir_all(path.parent().expect("sentinel parent")).expect("sentinel parent");
        fs::write(path, b"untouched").expect("sentinel");
    }
    let (version_url, version_sha1, mut requests) = spawn_preflight_failure_server().await;
    let downloader = test_manifest_downloader(&root, "preflight", &version_url, &version_sha1);
    let mut events = Vec::new();

    let error = downloader
        .install_version("preflight", |progress| events.push(progress))
        .await
        .expect_err("URL-less exact library must not install");

    assert!(error.to_string().contains("no download source"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let request_paths = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(request_paths, vec!["/version.json"]);
    assert!(events.iter().all(|event| {
        !matches!(
            event.phase.as_str(),
            "asset_index" | "java_runtime" | "java_runtime_ready" | "client_jar" | "libraries"
        )
    }));
    for path in [&client_path, &asset_index_path] {
        assert_eq!(fs::read(path).expect("unchanged sentinel"), b"untouched");
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn malformed_client_and_log_contracts_fail_before_install_effects() {
    let variants = [
        serde_json::json!({
            "id": "contract-preflight",
            "downloads": { "client": {
                "url": "https://example.invalid/client.jar",
                "sha1": "1111111111111111111111111111111111111111",
                "size": 0
            }}
        }),
        serde_json::json!({
            "id": "contract-preflight",
            "downloads": { "client": {
                "url": "https://example.invalid/client.jar",
                "sha1": "1111111111111111111111111111111111111111",
                "size": 7
            }},
            "logging": { "client": {
                "argument": "", "type": "log4j2-xml", "file": {
                    "id": "../escape.xml",
                    "url": "https://example.invalid/log.xml",
                    "sha1": "2222222222222222222222222222222222222222",
                    "size": 4
                }
            }}
        }),
    ];

    for (index, value) in variants.into_iter().enumerate() {
        let root = temp_dir(&format!("contract-preflight-{index}"));
        let sentinel = root.join("versions/contract-preflight/contract-preflight.json");
        fs::create_dir_all(sentinel.parent().expect("sentinel parent")).expect("sentinel parent");
        fs::write(&sentinel, b"untouched").expect("sentinel");
        let before = snapshot_tree(&root);
        let body = value.to_string().into_bytes();
        let version_url =
            spawn_download_response_server("200 OK", Vec::new(), body.clone(), 1).await;
        let downloader =
            test_manifest_downloader(&root, "contract-preflight", &version_url, &sha1_hex(&body));

        downloader
            .install_version("contract-preflight", |_| {})
            .await
            .expect_err("malformed authenticated contract must fail");

        assert_eq!(snapshot_tree(&root), before);
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn install_version_starts_asset_index_before_library_download_finishes() {
    let root = temp_dir("overlap-assets-libraries");
    let (version_url, version_sha1, mut requests, release_library) =
        spawn_overlapped_install_server().await;
    let downloader = test_manifest_downloader(&root, "overlap", &version_url, &version_sha1);
    let install = tokio::spawn(async move { downloader.install_version("overlap", |_| {}).await });

    let mut saw_asset_index = false;
    while !saw_asset_index {
        let path = timeout(Duration::from_secs(10), requests.recv())
            .await
            .expect("request should arrive before library release")
            .expect("request event");
        if path == "/asset-index.json" {
            saw_asset_index = true;
        }
    }

    release_library.send(()).expect("release library response");
    install
        .await
        .expect("install task should join")
        .expect("install should succeed");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn client_failure_cancels_a_blocked_library_lane() {
    assert_required_lane_failure_cancels_blocked_sibling("/client.jar", "/library.jar").await;
}

#[tokio::test]
async fn library_failure_aborts_and_drains_a_blocked_client_lane() {
    assert_required_lane_failure_cancels_blocked_sibling("/library.jar", "/client.jar").await;
}

#[tokio::test]
async fn sibling_failure_drains_blocked_nonruntime_workers_before_install_returns() {
    for checkpoint in [
        ManagedBlockingCheckpoint::LibraryValidation,
        ManagedBlockingCheckpoint::CacheHash,
        ManagedBlockingCheckpoint::SourceSpool,
    ] {
        assert_sibling_failure_drains_blocked_worker(checkpoint).await;
    }
}

#[tokio::test]
async fn simultaneous_client_and_asset_terminals_preserve_scheduler_precedence() {
    let version_id = "simultaneous-terminal-precedence";
    let root = temp_dir(version_id);
    let fixture = spawn_blocked_worker_install_server(version_id, true).await;
    fixture
        .release_client_failure
        .send(())
        .expect("release client terminal");
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone())
    .with_test_concurrent_terminal_wait();

    let error = timeout(
        Duration::from_secs(5),
        downloader.install_version(version_id, |_| {}),
    )
    .await
    .expect("simultaneous terminal coordinator must settle")
    .expect_err("client and asset terminals must fail");

    assert!(error.to_string().contains("/client.jar"));
    fixture.server.abort();
    let _ = fixture.server.await;
    let _ = fs::remove_dir_all(root);
}

async fn assert_sibling_failure_drains_blocked_worker(checkpoint: ManagedBlockingCheckpoint) {
    let version_id = format!("worker-drain-{checkpoint:?}").to_ascii_lowercase();
    let root = temp_dir(&version_id);
    let fixture = spawn_blocked_worker_install_server(&version_id, false).await;
    if checkpoint == ManagedBlockingCheckpoint::CacheHash {
        let cached_object = asset_object_path(&root, &fixture.object_hash);
        fs::create_dir_all(cached_object.parent().expect("cached object parent"))
            .expect("create cached object parent");
        fs::write(cached_object, &fixture.object_body).expect("seed exact cached asset object");
    }
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let release = Arc::new((StdMutex::new(false), Condvar::new()));
    let exited = Arc::new(AtomicUsize::new(0));
    let hook_release = Arc::clone(&release);
    let hook_exited = Arc::clone(&exited);
    let workers = ManagedBlockingWorkers::new_with_checkpoint_hook(Arc::new(move |observed| {
        if observed != checkpoint {
            return;
        }
        let Some(entered) = entered_tx.lock().expect("worker entered lock").take() else {
            return;
        };
        let _ = entered.send(());
        let (lock, condition) = &*hook_release;
        let released = lock.lock().expect("worker gate lock");
        drop(
            condition
                .wait_while(released, |released| !*released)
                .expect("worker gate wait"),
        );
        hook_exited.fetch_add(1, Ordering::Release);
    }));
    let downloader = test_manifest_downloader(
        &root,
        &version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone())
    .with_test_acquisition_workers(workers.clone());
    let install_version_id = version_id.clone();
    let mut install = tokio::spawn(async move {
        downloader
            .install_version(&install_version_id, |_| {})
            .await
    });

    timeout(Duration::from_secs(5), entered_rx)
        .await
        .expect("selected blocking worker must enter")
        .expect("blocking worker entered signal");
    fixture
        .release_client_failure
        .send(())
        .expect("release client failure");
    timeout(Duration::from_secs(2), async {
        while !workers.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("client failure must signal sibling cancellation");
    assert!(
        timeout(Duration::from_millis(25), &mut install)
            .await
            .is_err(),
        "install must remain in worker drain"
    );
    assert_eq!(exited.load(Ordering::Acquire), 0);

    let (lock, condition) = &*release;
    *lock.lock().expect("worker release lock") = true;
    condition.notify_one();
    let error = timeout(Duration::from_secs(5), install)
        .await
        .expect("drained install must return")
        .expect("install task")
        .expect_err("client failure must remain primary");
    assert!(!error.to_string().contains("cancelled"));
    assert_eq!(exited.load(Ordering::Acquire), 1);
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(exited.load(Ordering::Acquire), 1);
    assert!(!versions_dir(&root).join(&version_id).exists());

    fixture.server.abort();
    let _ = fixture.server.await;
    let _ = fs::remove_dir_all(root);
}

async fn assert_required_lane_failure_cancels_blocked_sibling(
    failing_path: &'static str,
    blocked_path: &'static str,
) {
    let version_id = if failing_path == "/client.jar" {
        "fail-fast-client"
    } else {
        "fail-fast-library"
    };
    let root = temp_dir(version_id);
    let fixture = spawn_fail_fast_install_server(version_id, failing_path, blocked_path).await;
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    );
    let install = tokio::spawn(async move { downloader.install_version(version_id, |_| {}).await });

    timeout(Duration::from_secs(2), fixture.blocked_started)
        .await
        .expect("the sibling lane must start before the primary failure")
        .expect("blocked sibling start signal");
    let error = timeout(Duration::from_secs(2), install)
        .await
        .expect("the primary lane failure must not wait for its blocked sibling")
        .expect("install task")
        .expect_err("the selected required lane must fail");
    assert!(matches!(
        error,
        DownloadError::Request(_) | DownloadError::ResolveManifest(_) | DownloadError::Integrity(_)
    ));
    timeout(Duration::from_secs(2), fixture.blocked_closed)
        .await
        .expect("the blocked sibling connection must close during failure drain")
        .expect("blocked sibling close signal");
    assert!(!versions_dir(&root).join(version_id).exists());

    fixture.server.abort();
    let _ = fixture.server.await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn p00_b09_contract_reconstruction_matches_install_without_touching_seeded_destinations() {
    let root = temp_dir("reconstruction-parity");
    let (version_url, version_sha1, mut requests) =
        spawn_reconstruction_parity_server("reconstruction").await;
    let downloader = test_manifest_downloader(&root, "reconstruction", &version_url, &version_sha1);
    let seeded = [
        root.join("versions/reconstruction/reconstruction.json"),
        root.join("versions/reconstruction/reconstruction.jar"),
        root.join("versions/reconstruction/untouched.sentinel"),
        root.join("libraries/org/example/exact/1.0.0/exact-1.0.0.jar"),
        root.join("libraries/org/example/observed/1.0.0/observed-1.0.0.jar"),
        root.join("libraries/org/example/observed-two/1.0.0/observed-two-1.0.0.jar"),
        root.join("assets/indexes/reconstruction-assets.json"),
        root.join("assets/log_configs/reconstruction-log.xml"),
        root.join("launcher_profiles.json"),
        root.join("cache/version_manifest_v2.json"),
        root.join("state/known-good/reconstruction.json"),
    ];
    for path in &seeded {
        fs::create_dir_all(path.parent().expect("seed parent")).expect("seed parent");
        fs::write(path, format!("sentinel:{}", path.display())).expect("seed sentinel");
    }
    let before = snapshot_tree(&root);

    let reconstruction = timeout(
        Duration::from_secs(10),
        downloader.reconstruct_version("reconstruction"),
    )
    .await
    .expect("reconstruction must not deadlock the scratch pool")
    .expect("reconstruct authenticated sources");

    assert_eq!(reconstruction.version_id(), "reconstruction");
    assert_eq!(snapshot_tree(&root), before);
    let reconstruction_requests =
        std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        reconstruction_requests
            .iter()
            .cloned()
            .collect::<HashSet<_>>(),
        HashSet::from([
            "/version.json".to_string(),
            "/libraries/observed.jar".to_string(),
            "/libraries/observed-two.jar".to_string(),
            "/asset-index.json".to_string(),
        ])
    );
    for path in [
        "/version.json",
        "/libraries/observed.jar",
        "/libraries/observed-two.jar",
        "/asset-index.json",
    ] {
        assert_eq!(
            reconstruction_requests
                .iter()
                .filter(|request| request.as_str() == path)
                .count(),
            1,
            "{path} must be fetched exactly once"
        );
    }
    let reconstructed = reconstruction.into_activation_source().into_parts();

    let guarded_root =
        crate::managed_fs::ManagedDir::open_root(&root).expect("guard retained test root");
    let retained_context = ManagedReconstructionContext::bind_libraries(guarded_root.clone())
        .await
        .expect("retained reconstruction context");
    let prepared = timeout(
        Duration::from_secs(10),
        downloader.reconstruct_version_authority("reconstruction", &retained_context),
    )
    .await
    .expect("retained reconstruction must not deadlock the shared source pool")
    .expect("reconstruct retained vanilla sources")
    .bind_managed_libraries(guarded_root, retained_context.take_library_cache_proofs())
    .expect("bind retained vanilla sources to final projection");
    assert_eq!(prepared.version_id(), "reconstruction");
    assert_eq!(prepared.library_entry_count(), 3);
    assert_eq!(prepared.retained_source_count(), 3);
    assert_eq!(
        prepared.expected_content_byte_count(),
        prepared.retained_content_byte_count()
    );
    assert_eq!(snapshot_tree(&root), before);
    let retained_requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    for path in [
        "/version.json",
        "/libraries/exact.jar",
        "/libraries/observed.jar",
        "/libraries/observed-two.jar",
        "/asset-index.json",
    ] {
        assert_eq!(
            retained_requests
                .iter()
                .filter(|request| request.as_str() == path)
                .count(),
            1,
            "retained reconstruction must fetch {path} exactly once"
        );
    }
    drop(prepared);

    let version_bundle = timeout(
        Duration::from_secs(10),
        downloader.reconstruct_version_authority(
            "reconstruction",
            &ManagedReconstructionContext::version_bundle(),
        ),
    )
    .await
    .expect("VersionBundle reconstruction must not deadlock")
    .expect("retain exact vanilla VersionBundle sources");
    assert!(version_bundle.retained_version_bundle_sources_match_projection());
    assert_eq!(snapshot_tree(&root), before);
    let version_bundle_requests =
        std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    for path in [
        "/version.json",
        "/client.jar",
        "/libraries/observed.jar",
        "/libraries/observed-two.jar",
        "/asset-index.json",
        "/log-config.xml",
    ] {
        assert_eq!(
            version_bundle_requests
                .iter()
                .filter(|request| request.as_str() == path)
                .count(),
            1,
            "VersionBundle reconstruction must fetch {path} exactly once"
        );
    }

    let installed = timeout(
        Duration::from_secs(10),
        downloader.install_version("reconstruction", |_| {}),
    )
    .await
    .expect("install must not deadlock the scratch pool")
    .expect("install identical authenticated sources")
    .into_activation_source()
    .into_parts();

    assert_eq!(installed, reconstructed);
    let [exact, observed, observed_two] = reconstruction_parity_library_bodies();
    let installed_libraries = [
        ("org/example/exact/1.0.0/exact-1.0.0.jar", exact),
        ("org/example/observed/1.0.0/observed-1.0.0.jar", observed),
        (
            "org/example/observed-two/1.0.0/observed-two-1.0.0.jar",
            observed_two,
        ),
    ];
    for (relative_path, expected_bytes) in &installed_libraries {
        assert_eq!(
            fs::read(root.join("libraries").join(relative_path))
                .expect("read replaced canonical library"),
            *expected_bytes,
            "install must replace the seeded corrupt {relative_path}"
        );
        let expected_sha1 = sha1_hex(expected_bytes);
        let entry = installed
            .1
            .entries()
            .iter()
            .find(|entry| entry.path().as_str() == *relative_path)
            .expect("installed library inventory entry");
        assert!(matches!(
            entry.integrity(),
            KnownGoodIntegrity::Sha1 { digest, size }
                if digest.as_str() == expected_sha1 && *size == expected_bytes.len() as u64
        ));
    }
    let install_requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    for path in [
        "/version.json",
        "/client.jar",
        "/libraries/observed.jar",
        "/libraries/observed-two.jar",
        "/asset-index.json",
        "/log-config.xml",
    ] {
        assert_eq!(
            install_requests
                .iter()
                .filter(|request| request.as_str() == path)
                .count(),
            1,
            "{path} must be fetched exactly once"
        );
    }
    assert_eq!(
        install_requests
            .iter()
            .filter(|request| request.as_str() == "/libraries/exact.jar")
            .count(),
        1,
        "the corrupt exact declaration must be retained once for replacement"
    );
    assert_settled_libraries_lane(&root);
    assert_settled_assets_lane(&root);
    assert_settled_version_bundle_lane(&root);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn p00_b09_contract_reconstruction_derives_runtime_inventory_without_runtime_effects() {
    let root = temp_dir("reconstruction-runtime");
    let (version_url, version_sha1, runtime_source, mut requests) =
        spawn_runtime_reconstruction_server("runtime-reconstruction").await;
    let downloader =
        test_manifest_downloader(&root, "runtime-reconstruction", &version_url, &version_sha1)
            .with_test_runtime_source(runtime_source);
    let sentinels = [
        root.join("versions/runtime-reconstruction/runtime-reconstruction.json"),
        root.join("versions/runtime-reconstruction/runtime-reconstruction.jar"),
        root.join("versions/runtime-reconstruction/untouched.sentinel"),
        root.join("runtime/jre-legacy/.axial-runtime-manifest.json"),
        root.join("runtime/jre-legacy/.axial-ready"),
        root.join("runtime/jre-legacy/bin/java"),
        root.join("runtime/jre-legacy/lib/data"),
        root.join("runtime/jre-legacy/java-link"),
    ];
    for path in &sentinels {
        fs::create_dir_all(path.parent().expect("runtime sentinel parent"))
            .expect("runtime sentinel parent");
        fs::write(path, b"runtime-sentinel").expect("runtime sentinel");
    }
    let before = snapshot_tree(&root);

    let (_, inventory) = downloader
        .reconstruct_version("runtime-reconstruction")
        .await
        .expect("runtime reconstruction")
        .into_activation_source()
        .into_parts();

    assert_eq!(snapshot_tree(&root), before);
    let kinds = inventory
        .entries()
        .iter()
        .map(|entry| entry.kind())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&KnownGoodArtifactKind::RuntimeManifestProof));
    assert!(kinds.contains(&KnownGoodArtifactKind::RuntimeReadyMarker));
    assert!(kinds.contains(&KnownGoodArtifactKind::RuntimeDirectory));
    assert!(kinds.contains(&KnownGoodArtifactKind::RuntimeExecutable));
    assert!(kinds.contains(&KnownGoodArtifactKind::RuntimeFile));
    assert!(kinds.contains(&KnownGoodArtifactKind::RuntimeLink));
    let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        requests.iter().cloned().collect::<HashSet<_>>(),
        HashSet::from([
            "/version.json".to_string(),
            "/runtime-manifest.json".to_string(),
        ])
    );
    assert!(!requests.iter().any(|path| path == "/runtime-file"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn normal_install_publishes_and_settles_three_member_version_bundle() {
    let version_id = "normal-three-member-success";
    let root = temp_dir(version_id);
    let (version_url, version_sha1, mut requests) =
        spawn_reconstruction_parity_server(version_id).await;
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

    let receipt = timeout(
        Duration::from_secs(10),
        downloader.install_version(version_id, |_| {}),
    )
    .await
    .expect("normal install should settle")
    .expect("normal install should succeed");

    assert_eq!(receipt.version_id(), version_id);
    assert_normal_bundle_contents(&root, version_id, true);
    assert_settled_libraries_lane(&root);
    assert_settled_assets_lane(&root);
    assert_settled_version_bundle_lane(&root);
    let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    for path in ["/version.json", "/client.jar", "/log-config.xml"] {
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.as_str() == path)
                .count(),
            1,
            "{path} must be fetched exactly once"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn p00_b09_contract_nonempty_assets_publish_once_and_match_reconstruction() {
    let version_id = "normal-nonempty-assets";
    let root = temp_dir(version_id);
    let mut fixture = spawn_nonempty_asset_install_server(version_id).await;
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone());
    let before = snapshot_tree(&root);

    let reconstructed = downloader
        .reconstruct_version(version_id)
        .await
        .expect("reconstruct nonempty asset authority")
        .into_activation_source()
        .into_parts();
    assert_eq!(snapshot_tree(&root), before);
    let reconstruction_requests = drain_request_paths(&mut fixture.requests);
    assert_eq!(request_count(&reconstruction_requests, "/version.json"), 1);
    assert_eq!(
        request_count(&reconstruction_requests, "/asset-index.json"),
        1
    );
    assert_eq!(request_count(&reconstruction_requests, "/client.jar"), 0);
    assert_eq!(
        request_count(&reconstruction_requests, &fixture.object_path),
        0
    );
    assert_eq!(
        request_count(&reconstruction_requests, &fixture.distinct_path),
        0
    );
    assert_eq!(
        request_count(&reconstruction_requests, &fixture.empty_path),
        0
    );

    let mut progress_events = Vec::new();
    let installed = downloader
        .install_version(version_id, |progress| progress_events.push(progress))
        .await
        .expect("install nonempty assets")
        .into_activation_source()
        .into_parts();
    assert_eq!(installed, reconstructed);
    let version_progress = progress_events
        .iter()
        .filter(|progress| progress.phase == "version_json")
        .map(|progress| (progress.current, progress.total))
        .collect::<Vec<_>>();
    assert_eq!(version_progress, [(0, 1), (1, 1)]);
    let asset_index_progress = progress_events
        .iter()
        .filter(|progress| progress.phase == "asset_index")
        .map(|progress| (progress.current, progress.total))
        .collect::<Vec<_>>();
    assert_eq!(asset_index_progress, [(1, 1)]);
    assert_eq!(
        progress_events
            .first()
            .map(|progress| progress.phase.as_str()),
        Some("version_json")
    );
    assert!(
        progress_events
            .iter()
            .filter(|progress| progress.phase == "asset_index")
            .all(|progress| progress.bytes_total.is_none()),
        "asset-index completion must not expose a partial denominator"
    );
    let expected_total = (fixture.asset_index.len()
        + fixture.version_json_size
        + b"nonempty-asset-client".len()
        + fixture.object.len()
        + fixture.distinct.len()) as u64;
    let stamped = progress_events
        .iter()
        .filter_map(|progress| progress.bytes_total.map(|total| (progress, total)))
        .collect::<Vec<_>>();
    assert!(!stamped.is_empty());
    assert!(stamped.iter().all(|(_, total)| *total == expected_total));
    assert!(stamped.iter().all(|(progress, total)| {
        progress
            .bytes_done
            .is_some_and(|completed| completed <= *total)
    }));
    let asset_entries = installed
        .1
        .entries()
        .iter()
        .filter(|entry| entry.root() == &KnownGoodRoot::Assets)
        .collect::<Vec<_>>();
    assert_eq!(asset_entries.len(), 4, "index plus three unique objects");
    assert_eq!(
        asset_entries
            .iter()
            .filter(|entry| entry.kind() == KnownGoodArtifactKind::AssetIndex)
            .count(),
        1
    );
    assert_eq!(
        asset_entries
            .iter()
            .filter(|entry| entry.kind() == KnownGoodArtifactKind::AssetObject)
            .count(),
        3
    );
    let mut asset_identities = asset_entries
        .iter()
        .map(|entry| {
            let (digest, size) = match entry.integrity() {
                KnownGoodIntegrity::Sha1 { digest, size }
                | KnownGoodIntegrity::ExactBytes { digest, size } => (digest.as_str(), *size),
                other => panic!("unexpected asset integrity: {other:?}"),
            };
            format!(
                "{}:{}:{digest}:{size}",
                entry.kind().stable_id(),
                entry.path().as_str()
            )
        })
        .collect::<Vec<_>>();
    asset_identities.sort();
    let mut expected_asset_identities = vec![
        format!(
            "asset_index:indexes/{}.json:{}:{}",
            fixture.asset_index_id,
            sha1_hex(&fixture.asset_index),
            fixture.asset_index.len()
        ),
        format!(
            "asset_object:objects/{}/{}:{}:{}",
            &fixture.object_hash[..2],
            fixture.object_hash,
            fixture.object_hash,
            fixture.object.len()
        ),
        format!(
            "asset_object:objects/{}/{}:{}:{}",
            &fixture.distinct_hash[..2],
            fixture.distinct_hash,
            fixture.distinct_hash,
            fixture.distinct.len()
        ),
        format!(
            "asset_object:objects/{}/{}:{}:0",
            &fixture.empty_hash[..2],
            fixture.empty_hash,
            fixture.empty_hash
        ),
    ];
    expected_asset_identities.sort();
    assert_eq!(asset_identities, expected_asset_identities);
    assert_eq!(
        fs::read(asset_index_path(&root, &fixture.asset_index_id)).expect("published asset index"),
        fixture.asset_index
    );
    assert_eq!(
        fs::read(asset_object_path(&root, &fixture.object_hash)).expect("published asset object"),
        fixture.object
    );
    assert_eq!(
        fs::read(asset_object_path(&root, &fixture.distinct_hash))
            .expect("published distinct asset object"),
        fixture.distinct
    );
    assert_eq!(
        fs::read(asset_object_path(&root, &fixture.empty_hash)).expect("published zero asset"),
        b""
    );
    assert!(
        !assets_dir(&root).join("virtual").exists(),
        "normal installation must not retain an install-time virtual-copy writer"
    );
    let install_requests = drain_request_paths(&mut fixture.requests);
    assert_eq!(request_count(&install_requests, &fixture.object_path), 1);
    assert_eq!(request_count(&install_requests, &fixture.distinct_path), 1);
    assert_eq!(request_count(&install_requests, &fixture.empty_path), 1);
    assert_settled_assets_lane(&root);
    assert_component_lane_absent(&root, "libraries");
    assert_settled_version_bundle_lane(&root);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn p00_b09_contract_normal_install_settles_assets_rollback() {
    let version_id = "normal-assets-first-row-rollback";
    let root = temp_dir(version_id);
    let fixture = spawn_nonempty_asset_install_server(version_id).await;
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone());
    roll_back_managed_install_component_after_first_row_for_test(
        version_id,
        crate::managed_component_table::ManagedComponentKind::Assets,
    );

    let error = timeout(
        Duration::from_secs(10),
        downloader.install_version(version_id, |_| {}),
    )
    .await
    .expect("Assets rollback install should settle")
    .expect_err("Assets rollback install should fail");

    assert!(
        error
            .to_string()
            .contains("managed Assets publication rolled back"),
        "unexpected Assets rollback error: {error}"
    );
    assert_settled_assets_lane(&root);
    assert_component_lane_absent(&root, "libraries");
    assert_component_lane_absent(&root, "version-bundle");
    assert!(!versions_dir(&root).join(version_id).exists());
    assert!(!asset_index_path(&root, &fixture.asset_index_id).exists());
    assert!(!asset_object_path(&root, &fixture.object_hash).exists());
    assert!(!asset_object_path(&root, &fixture.distinct_hash).exists());
    assert!(!asset_object_path(&root, &fixture.empty_hash).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn p00_b09_contract_normal_install_settles_libraries_rollback() {
    let version_id = "normal-libraries-first-row-rollback";
    let root = temp_dir(version_id);
    let (version_url, version_sha1, _) = spawn_reconstruction_parity_server(version_id).await;
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);
    roll_back_managed_install_component_after_first_row_for_test(
        version_id,
        crate::managed_component_table::ManagedComponentKind::Libraries,
    );

    let error = timeout(
        Duration::from_secs(10),
        downloader.install_version(version_id, |_| {}),
    )
    .await
    .expect("Libraries rollback install should settle")
    .expect_err("Libraries rollback install should fail");

    assert!(
        error
            .to_string()
            .contains("managed Libraries publication rolled back"),
        "unexpected Libraries rollback error: {error}"
    );
    assert_settled_assets_lane(&root);
    assert_settled_libraries_lane(&root);
    assert_component_lane_absent(&root, "version-bundle");
    assert!(!versions_dir(&root).join(version_id).exists());
    for library in [
        "org/example/exact/1.0.0/exact-1.0.0.jar",
        "org/example/observed/1.0.0/observed-1.0.0.jar",
        "org/example/observed-two/1.0.0/observed-two-1.0.0.jar",
    ] {
        assert!(!root.join("libraries").join(library).exists());
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn asset_cache_omits_exact_objects_and_replaces_same_size_corruption() {
    let version_id = "normal-asset-cache";
    let root = temp_dir(version_id);
    let mut fixture = spawn_nonempty_asset_install_server(version_id).await;
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone());

    downloader
        .install_version(version_id, |_| {})
        .await
        .expect("initial asset install");
    drain_request_paths(&mut fixture.requests);
    downloader
        .install_version(version_id, |_| {})
        .await
        .expect("exact-cache asset reinstall");
    let exact_requests = drain_request_paths(&mut fixture.requests);
    assert_eq!(request_count(&exact_requests, &fixture.object_path), 0);
    assert_eq!(request_count(&exact_requests, &fixture.distinct_path), 0);
    assert_eq!(request_count(&exact_requests, &fixture.empty_path), 0);

    let corrupt = vec![b'x'; fixture.object.len()];
    assert_ne!(corrupt, fixture.object);
    fs::write(asset_object_path(&root, &fixture.object_hash), &corrupt)
        .expect("seed same-size corrupt asset object");
    downloader
        .install_version(version_id, |_| {})
        .await
        .expect("corrupt-cache asset reinstall");
    let corrupt_requests = drain_request_paths(&mut fixture.requests);
    assert_eq!(request_count(&corrupt_requests, &fixture.object_path), 1);
    assert_eq!(request_count(&corrupt_requests, &fixture.distinct_path), 0);
    assert_eq!(request_count(&corrupt_requests, &fixture.empty_path), 0);
    assert_eq!(
        fs::read(asset_object_path(&root, &fixture.object_hash)).expect("replaced asset object"),
        fixture.object
    );
    assert_settled_assets_lane(&root);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn asset_sources_do_not_prewrite_while_the_shared_lease_is_held() {
    let version_id = "normal-assets-no-prewrite";
    let root = temp_dir(version_id);
    fs::create_dir_all(&root).expect("create asset no-prewrite root");
    let held_lease = ManagedRootPublicationLease::acquire(
        ManagedDir::open_root(&root).expect("open asset no-prewrite root"),
    )
    .await
    .expect("acquire held asset no-prewrite lease");
    let reached = observe_managed_install_lease_wait_for_test(version_id);
    let mut fixture = spawn_nonempty_asset_install_server(version_id).await;
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone());
    let install = tokio::spawn(async move { downloader.install_version(version_id, |_| {}).await });

    timeout(Duration::from_secs(10), reached)
        .await
        .expect("asset install should reach shared lease")
        .expect("asset install lease wait signal");
    assert!(!install.is_finished());
    assert!(!asset_index_path(&root, &fixture.asset_index_id).exists());
    assert!(!asset_object_path(&root, &fixture.object_hash).exists());
    assert!(!asset_object_path(&root, &fixture.distinct_hash).exists());
    assert!(!asset_object_path(&root, &fixture.empty_hash).exists());
    assert!(!versions_dir(&root).join(version_id).exists());
    let requests = drain_request_paths(&mut fixture.requests);
    assert_eq!(request_count(&requests, &fixture.object_path), 1);
    assert_eq!(request_count(&requests, &fixture.distinct_path), 1);
    assert_eq!(request_count(&requests, &fixture.empty_path), 1);

    install.abort();
    assert!(
        install
            .await
            .expect_err("outer asset no-prewrite install should be cancelled")
            .is_cancelled()
    );
    drop(held_lease);
    timeout(Duration::from_secs(10), async {
        loop {
            let object_matches = fs::read(asset_object_path(&root, &fixture.object_hash))
                .is_ok_and(|bytes| bytes == fixture.object);
            let distinct_matches = fs::read(asset_object_path(&root, &fixture.distinct_hash))
                .is_ok_and(|bytes| bytes == fixture.distinct);
            if object_matches
                && distinct_matches
                && asset_object_path(&root, &fixture.empty_hash).is_file()
                && assets_lane_is_settled(&root)
                && !root.join(".axial-publication/libraries").exists()
                && version_bundle_lane_is_settled(&root)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached nonempty asset publication should settle");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn asset_cache_drift_at_lease_admission_fails_before_install_effects() {
    let version_id = "normal-asset-cache-drift";
    let root = temp_dir(version_id);
    let mut fixture = spawn_nonempty_asset_install_server(version_id).await;
    let object_path = asset_object_path(&root, &fixture.object_hash);
    let distinct_path = asset_object_path(&root, &fixture.distinct_hash);
    let empty_path = asset_object_path(&root, &fixture.empty_hash);
    fs::create_dir_all(object_path.parent().expect("asset object parent"))
        .expect("create asset object parent");
    fs::create_dir_all(empty_path.parent().expect("empty asset parent"))
        .expect("create empty asset parent");
    fs::write(&object_path, &fixture.object).expect("seed exact cached asset");
    fs::create_dir_all(distinct_path.parent().expect("distinct asset parent"))
        .expect("create distinct asset parent");
    fs::write(&distinct_path, &fixture.distinct).expect("seed exact distinct cached asset");
    fs::write(&empty_path, b"").expect("seed exact cached empty asset");
    let held_lease = ManagedRootPublicationLease::acquire(
        ManagedDir::open_root(&root).expect("open asset cache-drift root"),
    )
    .await
    .expect("acquire held asset cache-drift lease");
    let reached = observe_managed_install_lease_wait_for_test(version_id);
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone());
    let install = tokio::spawn(async move { downloader.install_version(version_id, |_| {}).await });

    timeout(Duration::from_secs(10), reached)
        .await
        .expect("asset cache-drift install should reach lease")
        .expect("asset cache-drift lease wait signal");
    let corrupt = vec![b'x'; fixture.object.len()];
    fs::write(&object_path, &corrupt).expect("drift cached asset after admission");
    drop(held_lease);
    let error = install
        .await
        .expect("asset cache-drift install task")
        .expect_err("cache drift without a retained source must fail");

    assert!(error.to_string().contains("Assets"));
    assert_eq!(
        fs::read(&object_path).expect("drifted asset remains"),
        corrupt
    );
    assert!(!asset_index_path(&root, &fixture.asset_index_id).exists());
    assert!(!versions_dir(&root).join(version_id).exists());
    assert!(
        !root.join(".axial-publication/assets").exists(),
        "asset cache drift must fail before lane preparation"
    );
    assert!(!libraries_lane_is_settled(&root));
    assert!(!version_bundle_lane_is_settled(&root));
    let requests = drain_request_paths(&mut fixture.requests);
    assert_eq!(request_count(&requests, &fixture.object_path), 0);
    assert_eq!(request_count(&requests, &fixture.distinct_path), 0);
    assert_eq!(request_count(&requests, &fixture.empty_path), 0);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn managed_assets_reconstruction_rechecks_sparse_cache_under_publication_lease() {
    let version_id = "rebuild-asset-cache-drift";
    let root = temp_dir(version_id);
    let mut fixture = spawn_nonempty_asset_install_server(version_id).await;
    let object_path = asset_object_path(&root, &fixture.object_hash);
    let distinct_path = asset_object_path(&root, &fixture.distinct_hash);
    let empty_path = asset_object_path(&root, &fixture.empty_hash);
    for (path, bytes) in [
        (&object_path, fixture.object.as_slice()),
        (&distinct_path, fixture.distinct.as_slice()),
        (&empty_path, b"".as_slice()),
    ] {
        fs::create_dir_all(path.parent().expect("cached asset parent"))
            .expect("create cached asset parent");
        fs::write(path, bytes).expect("seed exact cached asset");
    }
    let guarded_root = ManagedDir::open_root(&root).expect("guard reconstruction root");
    let context = ManagedReconstructionContext::bind_assets(guarded_root.clone())
        .await
        .expect("bind Assets reconstruction");
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url.clone());
    let reconstruction = downloader
        .reconstruct_version_authority(version_id, &context)
        .await
        .expect("prepare sparse Assets reconstruction");
    let (sources, cache_proofs) = context
        .take_assets_authority()
        .expect("take sparse Assets authority");
    let prepared = reconstruction
        .bind_managed_assets(guarded_root, sources, cache_proofs)
        .expect("bind sparse Assets projection");
    assert_eq!(prepared.asset_entry_count(), 4);
    assert_eq!(
        prepared.retained_source_count(),
        1,
        "the authenticated index is retained while all exact objects are sparse"
    );
    let requests = drain_request_paths(&mut fixture.requests);
    assert_eq!(request_count(&requests, &fixture.object_path), 0);
    assert_eq!(request_count(&requests, &fixture.distinct_path), 0);
    assert_eq!(request_count(&requests, &fixture.empty_path), 0);

    let corrupt = vec![b'x'; fixture.object.len()];
    fs::write(&object_path, &corrupt).expect("drift sparse cached object");
    let (managed_root, receipt, sources) = prepared.into_effect_parts();
    let lease = ManagedRootPublicationLease::acquire(managed_root)
        .await
        .expect("acquire reconstruction publication lease");
    let projection = receipt
        .component_projection(crate::known_good::ManagedKnownGoodComponent::Assets)
        .expect("Assets projection");
    let result = crate::managed_component_lifecycle::publish_managed_component_effect(
        lease,
        projection,
        crate::managed_component_table::ManagedComponentKind::Assets,
        sources,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(
        fs::read(&object_path).expect("drifted object remains"),
        corrupt
    );
    assert!(!asset_index_path(&root, &fixture.asset_index_id).exists());
    assert!(
        !root.join(".axial-publication/assets").exists(),
        "stale sparse proof must fail before lifecycle intent or effects"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelling_install_aborts_blocked_asset_object_acquisition() {
    let version_id = "normal-assets-acquisition-cancel";
    let root = temp_dir(version_id);
    let fixture = spawn_blocked_asset_install_server(version_id).await;
    let downloader = test_manifest_downloader(
        &root,
        version_id,
        &fixture.version_url,
        &fixture.version_sha1,
    )
    .with_test_asset_object_base_url(fixture.object_base_url);
    let install = tokio::spawn(async move { downloader.install_version(version_id, |_| {}).await });

    timeout(Duration::from_secs(10), fixture.object_started)
        .await
        .expect("asset object request should start")
        .expect("asset object request signal");
    install.abort();
    assert!(
        install
            .await
            .expect_err("outer install should be cancelled")
            .is_cancelled()
    );
    timeout(Duration::from_secs(10), fixture.object_connection_closed)
        .await
        .expect("asset request connection should close after cancellation")
        .expect("asset request close signal");
    assert!(!asset_index_path(&root, &fixture.asset_index_id).exists());
    assert!(!asset_object_path(&root, &fixture.object_hash).exists());
    assert!(!versions_dir(&root).join(version_id).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn normal_reinstall_omits_exact_cached_library_source() {
    let version_id = "normal-exact-cache-reinstall";
    let root = temp_dir(version_id);
    let (version_url, version_sha1, mut requests) =
        spawn_reconstruction_parity_server(version_id).await;
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

    downloader
        .install_version(version_id, |_| {})
        .await
        .expect("initial normal install");
    while requests.try_recv().is_ok() {}

    downloader
        .install_version(version_id, |_| {})
        .await
        .expect("exact-cache reinstall");
    let reinstall_requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !reinstall_requests
            .iter()
            .any(|request| request == "/libraries/exact.jar"),
        "exact cached declaration must not be downloaded again"
    );
    for path in ["/libraries/observed.jar", "/libraries/observed-two.jar"] {
        assert_eq!(
            reinstall_requests
                .iter()
                .filter(|request| request.as_str() == path)
                .count(),
            1,
            "FreshStream {path} must be retained again"
        );
    }
    assert_settled_libraries_lane(&root);
    assert_settled_assets_lane(&root);
    assert_settled_version_bundle_lane(&root);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn normal_install_accepts_effect_failure_that_settles_committed() {
    let version_id = "normal-effect-committed";
    let root = temp_dir(version_id);
    let (version_url, version_sha1, _) = spawn_reconstruction_parity_server(version_id).await;
    crate::version_bundle_publication::fail_after_committed_outcome_for_test(version_id);
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

    let receipt = timeout(
        Duration::from_secs(10),
        downloader.install_version(version_id, |_| {}),
    )
    .await
    .expect("effect receipt settlement should terminate")
    .expect("committed settlement should succeed");

    assert_eq!(receipt.version_id(), version_id);
    assert_normal_bundle_contents(&root, version_id, true);
    assert_settled_version_bundle_lane(&root);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn normal_install_settles_crash_after_artifact_promotion_before_returning() {
    let version_id = "normal-crash-after-promotion";
    let root = temp_dir(version_id);
    let (version_url, version_sha1, _) = spawn_reconstruction_parity_server(version_id).await;
    crate::version_bundle_publication::crash_after_artifact_promotion_for_test(
        version_id,
        KnownGoodArtifactKind::ClientJar,
    );
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

    let error = timeout(
        Duration::from_secs(10),
        downloader.install_version(version_id, |_| {}),
    )
    .await
    .expect("crashed publication settlement should terminate")
    .expect_err("partially promoted bundle should settle rolled back");

    assert!(error.to_string().contains("rolled back"));
    let version_root = versions_dir(&root).join(version_id);
    assert!(!version_root.join(format!("{version_id}.json")).exists());
    assert!(!version_root.join(format!("{version_id}.jar")).exists());
    assert!(
        !assets_dir(&root)
            .join("log_configs/reconstruction-log.xml")
            .exists()
    );
    assert_settled_version_bundle_lane(&root);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn normal_install_rolls_back_bundle_replacements_before_returning_error() {
    let version_id = "normal-effect-rolled-back";
    let root = temp_dir(version_id);
    let version_root = versions_dir(&root).join(version_id);
    let log_path = assets_dir(&root).join("log_configs/reconstruction-log.xml");
    fs::create_dir_all(&version_root).expect("create previous version root");
    fs::create_dir_all(log_path.parent().expect("log parent")).expect("create previous log root");
    let previous_json = b"previous-version-json";
    let previous_client = b"previous-client-jar";
    let previous_log = b"previous-log-config";
    fs::write(
        version_root.join(format!("{version_id}.json")),
        previous_json,
    )
    .expect("seed previous version json");
    fs::write(
        version_root.join(format!("{version_id}.jar")),
        previous_client,
    )
    .expect("seed previous client jar");
    fs::write(&log_path, previous_log).expect("seed previous log config");
    let (version_url, version_sha1, _) = spawn_reconstruction_parity_server(version_id).await;
    crate::version_bundle_publication::fail_after_promotions_for_test(version_id, 2);
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

    let error = timeout(
        Duration::from_secs(10),
        downloader.install_version(version_id, |_| {}),
    )
    .await
    .expect("rollback settlement should terminate")
    .expect_err("rolled-back publication must not return a receipt");

    assert!(error.to_string().contains("rolled back"));
    assert_eq!(
        fs::read(version_root.join(format!("{version_id}.json"))).expect("restored version json"),
        previous_json
    );
    assert_eq!(
        fs::read(version_root.join(format!("{version_id}.jar"))).expect("restored client jar"),
        previous_client
    );
    assert_eq!(
        fs::read(log_path).expect("restored log config"),
        previous_log
    );
    assert_settled_version_bundle_lane(&root);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelling_normal_install_does_not_cancel_started_bundle_publication() {
    let version_id = "normal-publication-cancellation";
    let root = temp_dir(version_id);
    let (version_url, version_sha1, _) = spawn_reconstruction_parity_server(version_id).await;
    let (reached, release) =
        crate::version_bundle_publication::pause_after_promotions_for_test(version_id, 1);
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

    let install = tokio::spawn(async move { downloader.install_version(version_id, |_| {}).await });
    timeout(Duration::from_secs(10), reached)
        .await
        .expect("normal publication should reach its first promotion")
        .expect("normal publication pause signal");
    assert!(
        !install.is_finished(),
        "normal install must not return a receipt before publication settles"
    );
    install.abort();
    assert!(
        install
            .await
            .expect_err("outer install task should be cancelled")
            .is_cancelled()
    );
    release
        .send(())
        .expect("release detached normal publication");

    timeout(Duration::from_secs(10), async {
        loop {
            if normal_bundle_contents_match(&root, version_id, true)
                && assets_lane_is_settled(&root)
                && libraries_lane_is_settled(&root)
                && version_bundle_lane_is_settled(&root)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached normal publication should settle");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelling_normal_install_while_waiting_for_lease_detaches_publication_owner() {
    let version_id = "normal-lease-wait-cancellation";
    let root = temp_dir(version_id);
    fs::create_dir_all(&root).expect("create normal install root");
    let held_lease = ManagedRootPublicationLease::acquire(
        ManagedDir::open_root(&root).expect("open held normal install root"),
    )
    .await
    .expect("acquire held normal install lease");
    let reached = observe_managed_install_lease_wait_for_test(version_id);
    let (version_url, version_sha1, _) = spawn_reconstruction_parity_server(version_id).await;
    let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

    let install = tokio::spawn(async move { downloader.install_version(version_id, |_| {}).await });
    timeout(Duration::from_secs(10), reached)
        .await
        .expect("normal install should reach lease admission")
        .expect("normal install lease wait signal");
    assert!(
        !install.is_finished(),
        "normal install must wait for the lease"
    );
    install.abort();
    assert!(
        install
            .await
            .expect_err("lease waiter should be cancelled")
            .is_cancelled()
    );

    let probe = tokio::spawn(ManagedRootPublicationLease::acquire(
        ManagedDir::open_root(&root).expect("open probe normal install root"),
    ));
    drop(held_lease);
    let probe_lease = timeout(Duration::from_secs(10), probe)
        .await
        .expect("probe should acquire after detached publication")
        .expect("probe lease task should finish")
        .expect("probe lease should be acquired");

    drop(probe_lease);
    timeout(Duration::from_secs(10), async {
        loop {
            if normal_bundle_contents_match(&root, version_id, true)
                && assets_lane_is_settled(&root)
                && libraries_lane_is_settled(&root)
                && version_bundle_lane_is_settled(&root)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached lease waiter should publish and settle");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn normal_install_retries_local_version_bundle_settlement() {
    let cases = [
        (
            "normal-settlement-retry",
            crate::version_bundle_publication::fail_settlement_once_for_test as fn(&str),
        ),
        (
            "normal-settlement-marker-retry",
            crate::version_bundle_publication::fail_after_settlement_marker_for_test,
        ),
    ];

    for (version_id, inject_failure) in cases {
        let root = temp_dir(version_id);
        let (version_url, version_sha1, _) = spawn_reconstruction_parity_server(version_id).await;
        inject_failure(version_id);
        let downloader = test_manifest_downloader(&root, version_id, &version_url, &version_sha1);

        let receipt = timeout(
            Duration::from_secs(10),
            downloader.install_version(version_id, |_| {}),
        )
        .await
        .expect("local settlement retry should terminate")
        .expect("local settlement retry should preserve success");

        assert_eq!(receipt.version_id(), version_id);
        assert_normal_bundle_contents(&root, version_id, true);
        assert_settled_version_bundle_lane(&root);
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn runtime_effect_settles_before_artifact_failure_returns() {
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let source_receipt = authenticated_test_runtime_source(&JavaVersion {
        component: "java-runtime-delta".to_string(),
        major_version: 21,
    })
    .expect("authenticated runtime test source");
    let task = tokio::spawn(async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
        Ok::<RuntimeSourceReceipt, crate::runtime::JavaRuntimeLookupError>(source_receipt)
    });
    started_rx.await.expect("runtime task should start");
    let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());
    let mut settlement = tokio::spawn(async move {
        settle_runtime_pipeline_after_failure(
            Some(runtime_pipeline_for_test(task, true)),
            artifact_error,
        )
        .await
    });

    assert!(
        timeout(Duration::from_millis(25), &mut settlement)
            .await
            .is_err()
    );
    release_tx.send(()).expect("release runtime effect");
    let result = timeout(Duration::from_secs(1), settlement)
        .await
        .expect("settled runtime should release the primary error")
        .expect("runtime settlement task");

    assert!(matches!(
        result,
        DownloadError::ResolveManifest(message) if message == "artifact failed"
    ));
}

#[tokio::test]
async fn runtime_error_is_reported_when_runtime_settles() {
    let task = tokio::spawn(async {
        Err::<RuntimeSourceReceipt, _>(crate::runtime::JavaRuntimeLookupError::Install(
            "runtime failed".to_string(),
        ))
    });

    let result = settle_runtime_pipeline(Some(runtime_pipeline_for_test(task, false))).await;

    assert!(matches!(
        result,
        Err(DownloadError::PrepareRuntime(message))
            if message == "failed to install java runtime: runtime failed"
    ));
}

#[tokio::test]
async fn artifact_error_is_preserved_when_runtime_also_fails() {
    let task = tokio::spawn(async {
        Err::<RuntimeSourceReceipt, _>(crate::runtime::JavaRuntimeLookupError::Install(
            "runtime failed".to_string(),
        ))
    });
    let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());

    let result = settle_runtime_pipeline_after_failure(
        Some(runtime_pipeline_for_test(task, false)),
        artifact_error,
    )
    .await;

    assert!(matches!(
        result,
        DownloadError::ResolveManifest(message) if message == "artifact failed"
    ));
}

#[test]
fn runtime_ready_progress_uses_typed_phase_without_display_sniffing() {
    let progress = runtime_ensure_progress(
        &JavaVersion {
            component: "java-runtime-delta".to_string(),
            major_version: 21,
        },
        RuntimeEnsureEvent::ManagedRuntimeReady {
            component: "java-runtime-delta".to_string(),
        },
    );

    assert_eq!(progress.phase, "java_runtime_ready");
    assert_eq!(progress.current, 1);
    assert_eq!(progress.total, 1);
    assert_eq!(progress.file, None);
}

#[test]
fn mixed_windows_native_libraries_only_download_matching_arch() {
    let env = Environment {
        os_name: "windows".to_string(),
        os_arch: "x86_64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };
    let libraries = vec![
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-arm64"),
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-x86"),
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows"),
    ];

    let jobs = strict_library_jobs(&libraries, &env);
    let names = jobs.into_iter().map(|job| job.name).collect::<Vec<_>>();

    assert!(
        names
            .iter()
            .any(|name| name.contains("natives-windows.jar"))
    );
    assert!(!names.iter().any(|name| name.contains("arm64")));
    assert!(!names.iter().any(|name| name.contains("-x86.jar")));
}

#[test]
fn legacy_native_classifier_prefers_windows_generic_classifier() {
    let mut natives = HashMap::new();
    natives.insert("windows".to_string(), "natives-windows-${arch}".to_string());

    let mut classifiers = HashMap::new();
    classifiers.insert(
        "natives-windows".to_string(),
        artifact("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar"),
    );
    classifiers.insert(
        "natives-windows-arm64".to_string(),
        artifact("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows-arm64.jar"),
    );
    classifiers.insert(
        "natives-windows-x86".to_string(),
        artifact("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows-x86.jar"),
    );

    let lib = Library {
        name: "org.lwjgl:lwjgl:3.3.3".to_string(),
        downloads: Some(LibraryDownload {
            artifact: None,
            classifiers,
        }),
        natives,
        ..Library::default()
    };

    let env = Environment {
        os_name: "windows".to_string(),
        os_arch: "x86_64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };
    let job = strict_library_jobs(&[lib], &env)
        .into_iter()
        .next()
        .expect("native download");

    assert!(job.name.contains("natives-windows.jar"));
    assert!(!job.name.contains("arm64"));
    assert!(!job.name.contains("-x86.jar"));
}

#[test]
fn adaptive_download_concurrency_scales_with_bounds() {
    assert_eq!(adaptive_download_concurrency(1, 4, 16, 2), 4);
    assert_eq!(adaptive_download_concurrency(4, 4, 16, 2), 8);
    assert_eq!(adaptive_download_concurrency(32, 4, 16, 2), 16);
    assert_eq!(adaptive_download_concurrency(0, 8, 32, 4), 8);
}

#[test]
fn library_jobs_deduplicate_same_relative_path() {
    let env = Environment {
        os_name: "linux".to_string(),
        os_arch: "x86_64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };
    let libraries = vec![
        normal_library("org.example:duplicate:1.0.0"),
        normal_library("org.example:duplicate:1.0.0"),
    ];

    let jobs = strict_library_jobs(&libraries, &env);

    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].name.contains("duplicate-1.0.0.jar"));
}

#[test]
fn library_jobs_reject_conflicting_relative_path_contracts() {
    let env = Environment {
        os_name: "linux".to_string(),
        os_arch: "x86_64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };
    let path = "org/example/conflict/1.0.0/conflict-1.0.0.jar";
    let first = Library {
        name: "org.example:conflict:1.0.0".to_string(),
        downloads: Some(LibraryDownload {
            artifact: Some(artifact(path)),
            classifiers: HashMap::new(),
        }),
        ..Library::default()
    };
    let mut second = first.clone();
    second
        .downloads
        .as_mut()
        .and_then(|downloads| downloads.artifact.as_mut())
        .expect("artifact")
        .url = "https://mirror.invalid/conflict.jar".to_string();

    let error = library_jobs_for(&[first, second], &env).expect_err("conflicting plan");

    assert_eq!(error, LibraryPlanError::ConflictingArtifactPath);
}

#[test]
fn library_planning_rejects_unsafe_applicable_path() {
    let lib = Library {
        name: "org.example:unsafe:1.0.0".to_string(),
        downloads: Some(LibraryDownload {
            artifact: Some(LibraryArtifact {
                path: "../outside.jar".to_string(),
                url: "https://libraries.minecraft.net/outside.jar".to_string(),
                ..LibraryArtifact::default()
            }),
            classifiers: HashMap::new(),
        }),
        ..Library::default()
    };

    let error = library_verification_plans_for(
        Path::new("/tmp/axial-test"),
        &[lib],
        &crate::rules::default_environment(),
    )
    .expect_err("unsafe path");

    assert_eq!(error, LibraryPlanError::InvalidArtifactPath);
}

#[test]
fn url_less_library_is_inventory_and_verification_visible_but_not_downloadable() {
    let path = "org/example/offline/1.0.0/offline-1.0.0.jar";
    let lib = Library {
        name: "org.example:offline:1.0.0".to_string(),
        downloads: Some(LibraryDownload {
            artifact: Some(LibraryArtifact {
                path: path.to_string(),
                url: String::new(),
                ..LibraryArtifact::default()
            }),
            classifiers: HashMap::new(),
        }),
        ..Library::default()
    };
    let env = crate::rules::default_environment();

    let plans =
        library_artifact_plans_for(std::slice::from_ref(&lib), &env).expect("inventory plan");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].relative_path.as_str(), path);
    assert_eq!(plans[0].source_url, None);

    let verification_plans = library_verification_plans_for(
        Path::new("/tmp/axial-test"),
        std::slice::from_ref(&lib),
        &env,
    )
    .expect("verification plan");
    assert_eq!(verification_plans.len(), 1);
    assert_eq!(
        verification_plans[0].path,
        Path::new("/tmp/axial-test").join("libraries").join(path)
    );

    let error = library_jobs_for(&[lib], &env).expect_err("missing source");
    assert_eq!(error, LibraryPlanError::MissingDownloadSource);
}

#[test]
fn library_planning_rejects_invalid_nonempty_checksum() {
    let mut direct = normal_library("org.example:invalid-direct-checksum:1.0.0");
    direct.sha1 = "not-a-sha1".to_string();
    let mut legacy = normal_library("org.example:invalid-legacy-checksum:1.0.0");
    legacy.checksums = vec!["not-a-sha1".to_string()];

    for lib in [direct, legacy] {
        let error = library_jobs_for(&[lib], &crate::rules::default_environment())
            .expect_err("invalid checksum");

        assert_eq!(error, LibraryPlanError::InvalidChecksum);
    }
}

#[test]
fn native_selection_uses_supplied_environment_architecture() {
    let mut natives = HashMap::new();
    natives.insert("windows".to_string(), "natives-windows-${arch}".to_string());
    let mut classifiers = HashMap::new();
    classifiers.insert(
        "natives-windows-arm64".to_string(),
        artifact("org/example/native/1.0.0/native-1.0.0-arm64.jar"),
    );
    let lib = Library {
        name: "org.example:native:1.0.0".to_string(),
        downloads: Some(LibraryDownload {
            artifact: None,
            classifiers,
        }),
        natives,
        ..Library::default()
    };
    let env = Environment {
        os_name: "windows".to_string(),
        os_arch: "arm64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };

    let jobs = strict_library_jobs(&[lib], &env);

    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].name.ends_with("arm64.jar"));
}

#[test]
fn unique_asset_object_jobs_deduplicate_same_hash() {
    let hash_a = "abcdef1234567890abcdef1234567890abcdef12";
    let hash_b = "1234567890abcdef1234567890abcdef12345678";

    let jobs = unique_asset_object_jobs(16, [(hash_a, 4), (hash_a, 4), (hash_b, 8)])
        .expect("valid asset jobs");

    assert_eq!(jobs.len(), 2);
    let job_a = jobs.iter().find(|job| job.hash == hash_a).expect("hash a");
    let job_b = jobs.iter().find(|job| job.hash == hash_b).expect("hash b");
    assert_eq!(job_a.relative_path.as_str(), format!("objects/ab/{hash_a}"));
    assert_eq!(job_a.expected, ExpectedIntegrity::from_mojang(4, hash_a));
    assert_eq!(job_b.relative_path.as_str(), format!("objects/12/{hash_b}"));
    assert_eq!(job_b.expected, ExpectedIntegrity::from_mojang(8, hash_b));
}

#[test]
fn unique_asset_object_jobs_normalize_case_and_reject_conflicting_duplicate_sizes() {
    let lower = "abcdef1234567890abcdef1234567890abcdef12";
    let upper = lower.to_ascii_uppercase();
    let jobs = unique_asset_object_jobs(16, [(upper.as_str(), 4), (lower, 4)])
        .expect("case-normalized duplicate asset jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].hash, lower);

    let conflict = unique_asset_object_jobs(16, [(upper.as_str(), 4), (lower, 5)]);
    assert!(matches!(conflict, Err(DownloadError::Integrity(_))));
}

#[test]
fn unique_asset_object_jobs_reject_aggregate_overflow_before_acquisition() {
    let object_count = MAX_TIER2_AGGREGATE_BYTES / MAX_TIER2_ARTIFACT_BYTES;
    let declarations = (0..object_count)
        .map(|ordinal| {
            (
                format!("{ordinal:040x}"),
                i64::try_from(MAX_TIER2_ARTIFACT_BYTES).expect("per-artifact bound fits i64"),
            )
        })
        .collect::<Vec<_>>();
    let result = unique_asset_object_jobs(
        1,
        declarations
            .iter()
            .map(|(hash, size)| (hash.as_str(), *size)),
    );

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[test]
fn unique_asset_object_jobs_rejects_per_artifact_overflow_before_acquisition() {
    let hash = "abcdef1234567890abcdef1234567890abcdef12";
    let oversized = i64::try_from(MAX_TIER2_ARTIFACT_BYTES + 1).expect("bound fits i64");
    let result = unique_asset_object_jobs(1, [(hash, oversized)]);

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[test]
fn asset_index_requires_declared_object_size_but_accepts_explicit_zero() {
    let hash = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    assert!(
        parse_asset_index(format!(r#"{{"objects":{{"empty":{{"hash":"{hash}"}}}}}}"#).as_bytes())
            .is_err()
    );

    let index = parse_asset_index(
        format!(r#"{{"objects":{{"empty":{{"hash":"{hash}","size":0}}}}}}"#).as_bytes(),
    )
    .expect("explicit zero-size asset object");
    let jobs = unique_asset_object_jobs(
        16,
        index
            .objects
            .values()
            .map(|object| (object.hash.as_str(), object.size)),
    )
    .expect("zero-size asset job");

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].expected.size, Some(0));
}

#[test]
fn unique_asset_object_jobs_rejects_negative_size() {
    let hash = "abcdef1234567890abcdef1234567890abcdef12";
    let result = unique_asset_object_jobs(16, [(hash, -1)]);

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[test]
fn unique_asset_object_jobs_rejects_one_character_hash() {
    let result = unique_asset_object_jobs(16, [("a", 4)]);

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[test]
fn unique_asset_object_jobs_rejects_non_hex_hash() {
    let result = unique_asset_object_jobs(16, [("abcdef1234567890abcdef1234567890abcdef1z", 4)]);

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

fn checksumless_test_jar() -> Vec<u8> {
    test_jar("example/Entry.class", b"entry")
}

fn test_jar(entry_name: &str, entry_bytes: &[u8]) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    archive
        .start_file(entry_name, zip::write::SimpleFileOptions::default())
        .expect("start jar entry");
    std::io::Write::write_all(&mut archive, entry_bytes).expect("write jar entry");
    archive.finish().expect("finish jar").into_inner()
}

#[test]
fn execution_download_fact_labels_are_redacted() {
    let label = safe_download_target_label(Path::new(
        r"C:\Users\Alice\.minecraft\mods\secret-token -Xmx8192M.jar",
    ));
    let fact = execution_download_fact(
        ExecutionDownloadFactKind::ProviderFailure,
        &label,
        vec![("provider_payload", "{\"token\":\"secret\"}")],
    );
    let encoded = format!("{fact:?}");

    assert_eq!(fact.target, "artifact");
    for fragment in [
        "Users",
        "Alice",
        ".minecraft",
        "secret-token",
        "-Xmx",
        "provider_payload",
        "token",
        "secret",
    ] {
        assert!(
            !encoded.contains(fragment),
            "sensitive fragment survived: {fragment}"
        );
    }
}

#[test]
fn download_windows_verbatim_path_transform_handles_drive_unc_and_relative_paths() {
    assert_eq!(
        windows_verbatim_path_string(r"C:/Users/Alice/.minecraft/libraries/example.jar"),
        r"\\?\C:\Users\Alice\.minecraft\libraries\example.jar"
    );
    assert_eq!(
        windows_verbatim_path_string(r"\\server\share\libraries\example.jar"),
        r"\\?\UNC\server\share\libraries\example.jar"
    );
    assert_eq!(
        windows_verbatim_path_string(r"\\?\C:\already\verbatim.jar"),
        r"\\?\C:\already\verbatim.jar"
    );
    assert_eq!(
        windows_verbatim_path_string(r"libraries/example.jar"),
        r"libraries\example.jar"
    );
}

#[test]
fn download_integrity_futures_stay_small_enough_for_tokio_workers() {
    let path = Path::new("/tmp/axial-test/artifact.jar");

    assert!(
        std::mem::size_of_val(&hash_file(path)) < 4096,
        "hash_file future should not embed the hash buffer on the task stack"
    );
    let root = temp_dir("install-version-future-size");
    let runtime_cache =
        crate::ManagedRuntimeCache::isolated_for_test().expect("isolated downloader runtime cache");
    let downloader = Downloader::new(&root, runtime_cache);
    assert!(
        std::mem::size_of_val(&downloader.install_version("1.21.1", |_| {})) < 8192,
        "version-install future should stay comfortably below tokio worker stack limits"
    );
}

#[tokio::test]
async fn virtual_asset_copy_reports_destination_errors() {
    let root = temp_dir("virtual-asset-copy-error");
    let src = root.join("objects").join("aa").join("asset");
    let dst = root
        .join("virtual")
        .join("legacy")
        .join("sounds")
        .join("step.ogg");
    fs::create_dir_all(src.parent().expect("source parent")).expect("create source parent");
    fs::create_dir_all(&dst).expect("create destination directory");
    fs::write(&src, b"asset").expect("write source asset");

    let result = copy_virtual_asset_if_missing(&src, &dst).await;

    assert!(result.is_err());
    assert!(src.is_file());
    assert!(dst.is_dir());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn virtual_asset_copy_repairs_stale_existing_destination() {
    let root = temp_dir("virtual-asset-copy-existing");
    let src = root.join("objects").join("aa").join("asset");
    let dst = root
        .join("virtual")
        .join("legacy")
        .join("sounds")
        .join("step.ogg");
    fs::create_dir_all(src.parent().expect("source parent")).expect("create source parent");
    fs::create_dir_all(dst.parent().expect("destination parent"))
        .expect("create destination parent");
    fs::write(&src, b"source").expect("write source asset");
    fs::write(&dst, b"existing").expect("write existing virtual asset");

    copy_virtual_asset_if_missing(&src, &dst)
        .await
        .expect("stale virtual asset should be repaired");

    assert_eq!(
        fs::read(&dst).expect("read existing virtual asset"),
        b"source"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn virtual_asset_copy_keeps_matching_existing_destination() {
    let root = temp_dir("virtual-asset-copy-matching-existing");
    let src = root.join("objects").join("aa").join("asset");
    let dst = root
        .join("virtual")
        .join("legacy")
        .join("sounds")
        .join("step.ogg");
    fs::create_dir_all(src.parent().expect("source parent")).expect("create source parent");
    fs::create_dir_all(dst.parent().expect("destination parent"))
        .expect("create destination parent");
    fs::write(&src, b"source").expect("write source asset");
    fs::write(&dst, b"source").expect("write existing virtual asset");

    copy_virtual_asset_if_missing(&src, &dst)
        .await
        .expect("matching virtual asset should be kept");

    assert_eq!(
        fs::read(&dst).expect("read existing virtual asset"),
        b"source"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn virtual_asset_copy_reports_missing_source_object() {
    let root = temp_dir("virtual-asset-copy-missing-source");
    let src = root.join("objects").join("aa").join("asset");
    let dst = root
        .join("virtual")
        .join("legacy")
        .join("sounds")
        .join("step.ogg");

    let result = copy_virtual_asset_if_missing(&src, &dst).await;

    assert!(matches!(
        result,
        Err(DownloadError::Integrity(message))
            if message.contains("virtual asset source is missing")
    ));
    assert!(!dst.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn virtual_asset_mapping_copies_multiple_assets() {
    let root = temp_dir("virtual-asset-mapping-copy");
    let objects_dir = root.join("objects");
    let virtual_dir = root.join("virtual").join("legacy");
    let hash_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let hash_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    fs::create_dir_all(objects_dir.join("aa")).expect("create first object parent");
    fs::create_dir_all(objects_dir.join("bb")).expect("create second object parent");
    fs::write(objects_dir.join("aa").join(hash_a), b"step").expect("write first object");
    fs::write(objects_dir.join("bb").join(hash_b), b"hit").expect("write second object");

    copy_virtual_assets(
        &objects_dir,
        &virtual_dir,
        [
            ("sounds/step.ogg".to_string(), hash_a.to_string()),
            ("sounds/hit.ogg".to_string(), hash_b.to_string()),
        ],
    )
    .await
    .expect("copy virtual assets");

    assert_eq!(
        fs::read(virtual_dir.join("sounds").join("step.ogg")).expect("read first virtual asset"),
        b"step"
    );
    assert_eq!(
        fs::read(virtual_dir.join("sounds").join("hit.ogg")).expect("read second virtual asset"),
        b"hit"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn virtual_asset_mapping_rejects_unsafe_provider_paths() {
    let root = temp_dir("virtual-asset-mapping-unsafe");
    let objects_dir = root.join("objects");
    let virtual_dir = root.join("virtual").join("legacy");
    let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fs::create_dir_all(objects_dir.join("aa")).expect("create object parent");
    fs::write(objects_dir.join("aa").join(hash), b"asset").expect("write object");

    let result = copy_virtual_assets(
        &objects_dir,
        &virtual_dir,
        [("../escape.ogg".to_string(), hash.to_string())],
    )
    .await;

    assert!(matches!(
        result,
        Err(DownloadError::Integrity(message))
            if message.contains("unsafe virtual asset path")
    ));
    assert!(!root.join("escape.ogg").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn virtual_asset_mapping_reports_destination_errors() {
    let root = temp_dir("virtual-asset-mapping-destination-error");
    let objects_dir = root.join("objects");
    let virtual_dir = root.join("virtual").join("legacy");
    let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let dst = virtual_dir.join("sounds").join("step.ogg");
    fs::create_dir_all(objects_dir.join("aa")).expect("create object parent");
    fs::create_dir_all(&dst).expect("create destination directory");
    fs::write(objects_dir.join("aa").join(hash), b"asset").expect("write object");

    let result = copy_virtual_assets(
        &objects_dir,
        &virtual_dir,
        [("sounds/step.ogg".to_string(), hash.to_string())],
    )
    .await;

    assert!(result.is_err());
    assert!(dst.is_dir());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn virtual_asset_index_repair_refreshes_stale_legacy_copy() {
    let root = temp_dir("virtual-asset-index-repair");
    let asset = b"fresh";
    let hash = sha1_hex(asset);
    let object_path = root
        .join("assets")
        .join("objects")
        .join(&hash[..2])
        .join(&hash);
    let virtual_path = root
        .join("assets")
        .join("virtual")
        .join("legacy")
        .join("sounds")
        .join("step.ogg");
    let index_path = root.join("assets").join("indexes").join("legacy.json");
    fs::create_dir_all(object_path.parent().expect("object parent")).expect("create object parent");
    fs::create_dir_all(virtual_path.parent().expect("virtual parent"))
        .expect("create virtual parent");
    fs::create_dir_all(index_path.parent().expect("index parent")).expect("create index parent");
    fs::write(&object_path, asset).expect("write object");
    fs::write(&virtual_path, b"stale").expect("write stale virtual copy");
    fs::write(
        &index_path,
        format!(
            r#"{{
                "objects": {{
                    "sounds/step.ogg": {{ "hash": "{hash}", "size": {} }}
                }},
                "virtual": true
            }}"#,
            asset.len()
        ),
    )
    .expect("write asset index");

    let repaired = repair_virtual_assets_from_index(&root, &index_path)
        .await
        .expect("repair virtual assets");

    assert!(repaired);
    assert_eq!(fs::read(&virtual_path).expect("read virtual copy"), asset);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn virtual_asset_destination_rejects_unsafe_provider_paths() {
    let root = Path::new("/tmp/axial-test/assets/virtual/legacy");

    assert_eq!(
        virtual_asset_destination(root, "sounds/step.ogg").expect("safe path"),
        root.join("sounds").join("step.ogg")
    );

    for unsafe_name in [
        "",
        "/absolute.ogg",
        "../escape.ogg",
        "sounds/../escape.ogg",
        "sounds//step.ogg",
        "C:\\escape.ogg",
    ] {
        assert!(
            matches!(
                virtual_asset_destination(root, unsafe_name),
                Err(DownloadError::Integrity(message))
                    if message.contains("unsafe virtual asset path")
            ),
            "expected unsafe virtual asset path rejection for {unsafe_name:?}"
        );
    }
}

#[tokio::test]
async fn promotion_sweep_removes_stale_other_pid_backups_only() {
    let root = temp_dir("promote-stale-backup-sweep");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    fs::write(&destination, b"destination").expect("write destination");
    let other_pid = unused_pid_for_test(&[std::process::id()]);
    let other_pid_backup = root.join(format!("artifact.jar.axial-backup-{other_pid}"));
    let current_pid_backup = root.join(format!("artifact.jar.axial-backup-{}", std::process::id()));
    let unrelated = root.join("other.jar.axial-backup-7");
    let backup_directory = root.join("artifact.jar.axial-backup-8");
    let invalid_pid_backup = root.join("artifact.jar.axial-backup-not-a-pid");
    let malformed_suffix_backup = root.join(format!("artifact.jar.axial-backup-{other_pid}-extra"));
    fs::write(&other_pid_backup, b"stale").expect("write stale backup");
    fs::write(&current_pid_backup, b"current").expect("write current backup");
    fs::write(&unrelated, b"unrelated").expect("write unrelated backup");
    fs::write(&invalid_pid_backup, b"ambiguous").expect("write invalid pid backup");
    fs::write(&malformed_suffix_backup, b"ambiguous").expect("write malformed suffix backup");
    fs::create_dir_all(&backup_directory).expect("create backup-looking directory");

    sweep_stale_promotion_backups(&destination)
        .await
        .expect("sweep stale backups");

    assert!(destination.exists());
    assert!(!other_pid_backup.exists());
    assert!(current_pid_backup.exists());
    assert!(unrelated.exists());
    assert!(backup_directory.exists());
    assert!(invalid_pid_backup.exists());
    assert!(malformed_suffix_backup.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn promotion_sweep_preserves_live_other_pid_backup() {
    let root = temp_dir("promote-live-backup-sweep");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    fs::write(&destination, b"destination").expect("write destination");
    let mut child = spawn_promotion_sweep_child_process();
    let live_pid_backup = root.join(format!("artifact.jar.axial-backup-{}", child.id()));
    fs::write(&live_pid_backup, b"live").expect("write live backup");

    let sweep_result = sweep_stale_promotion_backups(&destination).await;
    let destination_exists = destination.exists();
    let live_pid_backup_exists = live_pid_backup.exists();
    let _ = child.kill();
    let _ = child.wait();

    sweep_result.expect("sweep stale backups");
    assert!(destination_exists);
    assert!(live_pid_backup_exists);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn promote_sweeps_stale_backups_before_replace() {
    let root = temp_dir("promote-sweeps-before-replace");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    let temp_path = root.join("source-temp-sentinel");
    let other_pid = unused_pid_for_test(&[std::process::id()]);
    let stale_backup = root.join(format!("artifact.jar.axial-backup-{other_pid}"));
    fs::write(&destination, b"stale").expect("write destination");
    fs::write(&temp_path, b"fresh").expect("write temp");
    fs::write(&stale_backup, b"orphan").expect("write stale backup");

    super::transfer::promote_launcher_managed_artifact_temp_once(&temp_path, &destination)
        .await
        .expect("promote temp");

    assert_eq!(
        fs::read(&destination).expect("read promoted artifact"),
        b"fresh"
    );
    assert!(!stale_backup.exists());
    assert!(!temp_path.exists());

    let _ = fs::remove_dir_all(root);
}

fn spawn_promotion_sweep_child_process() -> std::process::Child {
    std::process::Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("download::tests::promotion_sweep_live_pid_child_process")
        .arg("--ignored")
        .env("AXIAL_PROMOTION_SWEEP_CHILD", "1")
        .spawn()
        .expect("spawn live pid child")
}

#[test]
#[ignore]
fn promotion_sweep_live_pid_child_process() {
    if std::env::var_os("AXIAL_PROMOTION_SWEEP_CHILD").is_some() {
        std::thread::sleep(Duration::from_secs(30));
    }
}

fn unused_pid_for_test(excluded: &[u32]) -> u32 {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    (1..=1_000_000)
        .find(|pid| {
            !excluded.contains(pid) && system.process(sysinfo::Pid::from_u32(*pid)).is_none()
        })
        .expect("unused pid")
}

#[tokio::test]
async fn remove_stale_download_temp_removes_directory() {
    let root = temp_dir("temp-cleanup-dir");
    fs::create_dir_all(root.join("artifact.tmp")).expect("create stale temp directory");

    remove_stale_download_temp(&root.join("artifact.tmp"))
        .await
        .expect("remove stale temp directory");

    assert!(!root.join("artifact.tmp").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn remove_stale_download_temp_removes_file() {
    let root = temp_dir("temp-cleanup-file");
    fs::create_dir_all(&root).expect("create root");
    fs::write(root.join("artifact.tmp"), b"stale").expect("write stale temp file");

    remove_stale_download_temp(&root.join("artifact.tmp"))
        .await
        .expect("remove stale temp file");

    assert!(!root.join("artifact.tmp").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn remove_stale_download_temp_accepts_missing_path() {
    let root = temp_dir("temp-cleanup-missing");

    remove_stale_download_temp(&root.join("artifact.tmp"))
        .await
        .expect("missing temp path is clean");

    assert!(!root.join("artifact.tmp").exists());
}

#[tokio::test]
async fn promote_launcher_managed_artifact_temp_once_preserves_destination_when_temp_is_missing() {
    let root = temp_dir("promote-missing-temp");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    let temp_path = root.join("missing.tmp");
    fs::write(&destination, b"existing").expect("write existing artifact");

    let result = promote_launcher_managed_artifact_temp_once(&temp_path, &destination).await;

    assert!(result.is_err());
    assert_eq!(
        fs::read(&destination).expect("read existing artifact"),
        b"existing"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn library_artifact_job_carries_expected_integrity() {
    let artifact_path = "org/example/lib/1.0.0/lib-1.0.0.jar";
    let sha1 = "abcdef1234567890abcdef1234567890abcdef12";
    let lib = Library {
        name: "org.example:lib:1.0.0".to_string(),
        downloads: Some(LibraryDownload {
            artifact: Some(LibraryArtifact {
                path: artifact_path.to_string(),
                url: format!("https://libraries.minecraft.net/{artifact_path}"),
                sha1: sha1.to_string(),
                size: 1234,
            }),
            classifiers: HashMap::new(),
        }),
        ..Library::default()
    };

    let job = strict_library_jobs(&[lib], &crate::rules::default_environment())
        .into_iter()
        .next()
        .expect("library job");

    assert_eq!(job.expected, ExpectedIntegrity::from_mojang(1234, sha1));
}

#[test]
fn library_job_uses_legacy_checksums_when_mojang_sha1_is_missing() {
    let artifact_path = "org/example/lib/1.0.0/lib-1.0.0.jar";
    let sha1 = "abcdef1234567890abcdef1234567890abcdef12";
    let lib = Library {
        name: "org.example:lib:1.0.0".to_string(),
        downloads: Some(LibraryDownload {
            artifact: Some(LibraryArtifact {
                path: artifact_path.to_string(),
                url: format!("https://libraries.minecraft.net/{artifact_path}"),
                sha1: String::new(),
                size: 0,
            }),
            classifiers: HashMap::new(),
        }),
        checksums: vec![sha1.to_string()],
        ..Library::default()
    };

    let job = strict_library_jobs(&[lib], &crate::rules::default_environment())
        .into_iter()
        .next()
        .expect("library job");

    assert_eq!(job.expected, ExpectedIntegrity::from_sha1(sha1));
}

#[test]
fn native_classifier_job_carries_expected_integrity() {
    let artifact_path = "org/example/lib/1.0.0/lib-1.0.0-natives-windows.jar";
    let sha1 = "1234567890abcdef1234567890abcdef12345678";
    let mut natives = HashMap::new();
    natives.insert("windows".to_string(), "natives-windows".to_string());
    let mut classifiers = HashMap::new();
    classifiers.insert(
        "natives-windows".to_string(),
        LibraryArtifact {
            path: artifact_path.to_string(),
            url: format!("https://libraries.minecraft.net/{artifact_path}"),
            sha1: sha1.to_string(),
            size: 4321,
        },
    );
    let lib = Library {
        name: "org.example:lib:1.0.0".to_string(),
        downloads: Some(LibraryDownload {
            artifact: None,
            classifiers,
        }),
        natives,
        ..Library::default()
    };

    let env = Environment {
        os_name: "windows".to_string(),
        os_arch: "x86_64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };
    let job = strict_library_jobs(&[lib], &env)
        .into_iter()
        .next()
        .expect("native job");

    assert_eq!(job.expected, ExpectedIntegrity::from_mojang(4321, sha1));
}

#[test]
fn library_maven_fallback_job_reuses_when_metadata_missing() {
    let lib = Library {
        name: "org.example:lib:1.0.0".to_string(),
        downloads: None,
        ..Library::default()
    };

    let job = strict_library_jobs(&[lib], &crate::rules::default_environment())
        .into_iter()
        .next()
        .expect("library job");

    assert_eq!(job.expected, ExpectedIntegrity::default());
    assert!(!job.expected.has_evidence());
}

#[test]
fn expected_integrity_ignores_default_mojang_metadata() {
    let expected = ExpectedIntegrity::from_mojang(0, " ");

    assert_eq!(expected, ExpectedIntegrity::default());
    assert!(!expected.has_evidence());
}

#[tokio::test]
async fn install_version_rejects_unlisted_local_version_json() {
    let root = temp_dir("unlisted-local-version-json");
    let version_dir = versions_dir(&root).join("custom");
    fs::create_dir_all(&version_dir).expect("create version directory");
    fs::write(version_dir.join("custom.json"), br#"{"id":"custom"}"#)
        .expect("write local version json");
    let error = Downloader::with_test_install_manifest(&root, empty_test_install_manifest())
        .install_version("custom", |_| {})
        .await
        .expect_err("unlisted local metadata must not become install authority");

    assert!(error.to_string().contains("not found in manifest"));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_version_rejects_unsafe_identity_before_filesystem_effects() {
    let root = temp_dir("unsafe-version-identity");
    let absolute = root
        .with_file_name(format!(
            "{}-absolute",
            root.file_name()
                .and_then(|value| value.to_str())
                .expect("temporary root file name")
        ))
        .to_string_lossy()
        .to_string();
    let traversal_name = format!(
        "{}-escape",
        root.file_name()
            .and_then(|value| value.to_str())
            .expect("temporary root file name")
    );
    let traversal_id = format!("../{traversal_name}");
    let traversal_target = root
        .parent()
        .expect("temporary root parent")
        .join(&traversal_name);
    let oversized =
        "a".repeat(crate::portable_path::MAX_PORTABLE_FILE_NAME_BYTES - ".json".len() + 1);

    for version_id in [traversal_id.as_str(), absolute.as_str(), oversized.as_str()] {
        let mut events = Vec::new();
        let error = Downloader::with_test_install_manifest(&root, empty_test_install_manifest())
            .install_version(version_id, |progress| events.push(progress))
            .await
            .expect_err("unsafe version identity must fail");

        assert!(
            error
                .to_string()
                .contains("invalid Minecraft version identity")
        );
        assert_eq!(events.len(), 1);
        assert!(events[0].done);
        assert_eq!(events[0].phase, "error");
        assert!(!root.exists());
        assert!(!traversal_target.exists());
        assert!(!Path::new(&absolute).exists());
    }
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn strict_library_jobs(libraries: &[Library], env: &Environment) -> Vec<DownloadJob> {
    library_jobs_for(libraries, env).expect("valid library plan")
}

fn native_library(name: &str) -> Library {
    let artifact_path = maven_to_path(name).to_string_lossy().replace('\\', "/");
    Library {
        name: name.to_string(),
        downloads: Some(LibraryDownload {
            artifact: Some(artifact(&artifact_path)),
            classifiers: HashMap::new(),
        }),
        ..Library::default()
    }
}

fn normal_library(name: &str) -> Library {
    let artifact_path = maven_to_path(name).to_string_lossy().replace('\\', "/");
    Library {
        name: name.to_string(),
        downloads: Some(LibraryDownload {
            artifact: Some(artifact(&artifact_path)),
            classifiers: HashMap::new(),
        }),
        ..Library::default()
    }
}

fn artifact(path: &str) -> LibraryArtifact {
    LibraryArtifact {
        path: path.to_string(),
        url: format!("https://libraries.minecraft.net/{path}"),
        ..LibraryArtifact::default()
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "axial-download-{prefix}-{}-{nanos:x}",
        std::process::id()
    ))
}

#[derive(Debug, Eq, PartialEq)]
enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
    Link(PathBuf),
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries {
            let entry = entry.expect("snapshot entry");
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("snapshot metadata");
            let relative = path
                .strip_prefix(root)
                .expect("snapshot relative")
                .to_path_buf();
            if metadata.is_dir() {
                snapshot.insert(relative, SnapshotEntry::Directory);
                visit(root, &path, snapshot);
            } else if metadata.file_type().is_symlink() {
                snapshot.insert(
                    relative,
                    SnapshotEntry::Link(fs::read_link(&path).expect("snapshot link")),
                );
            } else {
                snapshot.insert(
                    relative,
                    SnapshotEntry::File(fs::read(&path).expect("snapshot file")),
                );
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn assert_normal_bundle_contents(root: &Path, version_id: &str, with_log_config: bool) {
    let version_root = versions_dir(root).join(version_id);
    let version_json =
        fs::read(version_root.join(format!("{version_id}.json"))).expect("published version json");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&version_json)
            .expect("published version metadata")["id"]
            .as_str(),
        Some(version_id)
    );
    assert_eq!(
        fs::read(version_root.join(format!("{version_id}.jar"))).expect("published client jar"),
        b"reconstruction-client"
    );
    if with_log_config {
        assert_eq!(
            fs::read(assets_dir(root).join("log_configs/reconstruction-log.xml"))
                .expect("published log config"),
            b"<log4j/>"
        );
    }
}

fn normal_bundle_contents_match(root: &Path, version_id: &str, with_log_config: bool) -> bool {
    let version_root = versions_dir(root).join(version_id);
    let version_matches = fs::read(version_root.join(format!("{version_id}.json")))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|version| version["id"].as_str() == Some(version_id));
    let client_matches = fs::read(version_root.join(format!("{version_id}.jar")))
        .is_ok_and(|bytes| bytes == b"reconstruction-client");
    let log_matches = !with_log_config
        || fs::read(assets_dir(root).join("log_configs/reconstruction-log.xml"))
            .is_ok_and(|bytes| bytes == b"<log4j/>");
    version_matches && client_matches && log_matches
}

fn assert_settled_version_bundle_lane(root: &Path) {
    assert!(
        version_bundle_lane_is_settled(root),
        "version bundle lane must be terminally settled"
    );
}

fn assert_settled_libraries_lane(root: &Path) {
    assert!(
        libraries_lane_is_settled(root),
        "Libraries lane must be terminally settled"
    );
}

fn assert_settled_assets_lane(root: &Path) {
    assert!(
        assets_lane_is_settled(root),
        "Assets lane must be terminally settled"
    );
}

fn assert_component_lane_absent(root: &Path, name: &str) {
    assert!(
        !root.join(".axial-publication").join(name).exists(),
        "{name} no-effect projection must not create a publication lane"
    );
}

fn assets_lane_is_settled(root: &Path) -> bool {
    component_lane_is_settled(root, "assets")
}

fn libraries_lane_is_settled(root: &Path) -> bool {
    component_lane_is_settled(root, "libraries")
}

fn component_lane_is_settled(root: &Path, name: &str) -> bool {
    let lane = root.join(".axial-publication").join(name);
    let Ok(entries) = fs::read_dir(&lane) else {
        return false;
    };
    let mut names = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
        == vec![
            "ancestors".to_string(),
            "quarantine".to_string(),
            "staging".to_string(),
            "table".to_string(),
        ]
        && ["quarantine", "staging", "table"].into_iter().all(|name| {
            fs::read_dir(lane.join(name)).is_ok_and(|mut entries| entries.next().is_none())
        })
        && ["records", "staging"].into_iter().all(|name| {
            fs::read_dir(lane.join("ancestors").join(name))
                .is_ok_and(|mut entries| entries.next().is_none())
        })
}

fn version_bundle_lane_is_settled(root: &Path) -> bool {
    let lane = root.join(".axial-publication/version-bundle");
    let Ok(entries) = fs::read_dir(&lane) else {
        return false;
    };
    let mut names = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names == vec!["quarantine".to_string(), "staging".to_string()]
        && fs::read_dir(lane.join("quarantine")).is_ok_and(|mut entries| entries.next().is_none())
        && fs::read_dir(lane.join("staging")).is_ok_and(|mut entries| entries.next().is_none())
}

async fn spawn_download_response_server(
    status: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    responses: usize,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind download response server");
    let url = format!("http://{}", listener.local_addr().expect("local addr"));
    let status = status.to_string();
    tokio::spawn(async move {
        for _ in 0..responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let _ = read_request_path(&mut socket).await;
            let mut response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
            for (name, value) in &headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        }
    });
    url
}

async fn spawn_overlapped_install_server() -> (
    String,
    String,
    mpsc::UnboundedReceiver<String>,
    oneshot::Sender<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind install overlap server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let (release_library_tx, release_library_rx) = oneshot::channel();
    let library_body = test_jar(
        "org/example/overlap/OverlapLibrary.class",
        b"overlapped-library",
    );
    let library_sha1 = sha1_hex(&library_body);
    let client_body = b"client".to_vec();
    let client_sha1 = sha1_hex(&client_body);
    let asset_index_body = br#"{"objects":{}}"#.to_vec();
    let asset_index_sha1 = sha1_hex(&asset_index_body);
    let version_body = serde_json::json!({
        "id": "overlap",
        "downloads": {
            "client": {
                "url": format!("{base_url}/client.jar"),
                "sha1": client_sha1,
                "size": client_body.len()
            }
        },
        "assetIndex": {
            "id": "overlap-assets",
            "sha1": asset_index_sha1,
            "size": asset_index_body.len(),
            "url": format!("{base_url}/asset-index.json")
        },
        "libraries": [{
            "name": "org.example:lib:1.0.0",
            "downloads": {
                "artifact": {
                    "path": "org/example/lib/1.0.0/lib-1.0.0.jar",
                    "url": format!("{base_url}/libraries/lib.jar"),
                    "sha1": library_sha1,
                    "size": library_body.len()
                }
            }
        }]
    })
    .to_string()
    .into_bytes();
    let version_sha1 = sha1_hex(&version_body);

    tokio::spawn(async move {
        let release_library_rx = Arc::new(Mutex::new(Some(release_library_rx)));
        for _ in 0..5 {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request_path = match read_request_path(&mut socket).await {
                Some(path) => path,
                None => return,
            };
            let _ = request_tx.send(request_path.clone());
            let body = match request_path.as_str() {
                "/version.json" => version_body.clone(),
                "/asset-index.json" => asset_index_body.clone(),
                "/client.jar" => client_body.clone(),
                "/libraries/lib.jar" => {
                    let release_library_rx = Arc::clone(&release_library_rx);
                    let library_body = library_body.clone();
                    tokio::spawn(async move {
                        if let Some(receiver) = release_library_rx.lock().await.take() {
                            let _ = receiver.await;
                        }
                        write_raw_response(&mut socket, "200 OK", &library_body).await;
                    });
                    continue;
                }
                _ => {
                    write_raw_response(&mut socket, "404 Not Found", b"not found").await;
                    continue;
                }
            };
            write_raw_response(&mut socket, "200 OK", &body).await;
        }
    });

    (
        format!("{base_url}/version.json"),
        version_sha1,
        request_rx,
        release_library_tx,
    )
}

struct FailFastInstallServer {
    version_url: String,
    version_sha1: String,
    blocked_started: oneshot::Receiver<()>,
    blocked_closed: oneshot::Receiver<()>,
    server: tokio::task::JoinHandle<()>,
}

struct BlockedWorkerInstallServer {
    version_url: String,
    version_sha1: String,
    object_base_url: String,
    object_body: Vec<u8>,
    object_hash: String,
    release_client_failure: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

async fn spawn_blocked_worker_install_server(
    version_id: &str,
    fail_object: bool,
) -> BlockedWorkerInstallServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blocked-worker install server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let client_body = b"blocked-worker client".to_vec();
    let library_body = test_jar(
        "org/example/worker/WorkerLibrary.class",
        b"blocked-worker library",
    );
    let object_body = b"blocked-worker asset".to_vec();
    let object_hash = sha1_hex(&object_body);
    let asset_index = serde_json::to_vec(&serde_json::json!({
        "objects": {
            "worker/asset.bin": {
                "hash": object_hash,
                "size": object_body.len()
            }
        }
    }))
    .expect("serialize blocked-worker asset index");
    let version_body = serde_json::to_vec(&serde_json::json!({
        "id": version_id,
        "assetIndex": {
            "id": format!("{version_id}-assets"),
            "url": format!("{base_url}/asset-index.json"),
            "sha1": sha1_hex(&asset_index),
            "size": asset_index.len(),
            "totalSize": object_body.len()
        },
        "downloads": {
            "client": {
                "url": format!("{base_url}/client.jar"),
                "sha1": sha1_hex(&client_body),
                "size": client_body.len()
            }
        },
        "libraries": [{
            "name": "org.example:worker:1.0.0",
            "downloads": {
                "artifact": {
                    "path": "org/example/worker/1.0.0/worker-1.0.0.jar",
                    "url": format!("{base_url}/library.jar"),
                    "sha1": sha1_hex(&library_body),
                    "size": library_body.len()
                }
            }
        }]
    }))
    .expect("serialize blocked-worker version");
    let version_sha1 = sha1_hex(&version_body);
    let object_path = format!("/{}/{}", &object_hash[..2], object_hash);
    let returned_object_body = object_body.clone();
    let (release_client_failure, release_client_failure_rx) = oneshot::channel();
    let release_client_failure_rx = Arc::new(Mutex::new(Some(release_client_failure_rx)));

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let version_body = version_body.clone();
            let asset_index = asset_index.clone();
            let library_body = library_body.clone();
            let object_body = object_body.clone();
            let object_path = object_path.clone();
            let release_client_failure_rx = Arc::clone(&release_client_failure_rx);
            tokio::spawn(async move {
                let Some(path) = read_request_path(&mut socket).await else {
                    return;
                };
                match path.as_str() {
                    "/version.json" => {
                        write_raw_response(&mut socket, "200 OK", &version_body).await
                    }
                    "/asset-index.json" => {
                        write_raw_response(&mut socket, "200 OK", &asset_index).await
                    }
                    "/library.jar" => {
                        write_raw_response(&mut socket, "200 OK", &library_body).await
                    }
                    "/client.jar" => {
                        let release = release_client_failure_rx.lock().await.take();
                        if let Some(release) = release {
                            let _ = release.await;
                        }
                        write_raw_response(&mut socket, "404 Not Found", b"missing").await;
                    }
                    path if path == object_path => {
                        if fail_object {
                            write_raw_response(&mut socket, "404 Not Found", b"missing").await
                        } else {
                            write_raw_response(&mut socket, "200 OK", &object_body).await
                        }
                    }
                    _ => write_raw_response(&mut socket, "404 Not Found", b"missing").await,
                }
            });
        }
    });

    BlockedWorkerInstallServer {
        version_url: format!("{base_url}/version.json"),
        version_sha1,
        object_base_url: base_url,
        object_body: returned_object_body,
        object_hash,
        release_client_failure,
        server,
    }
}

async fn spawn_fail_fast_install_server(
    version_id: &str,
    failing_path: &'static str,
    blocked_path: &'static str,
) -> FailFastInstallServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fail-fast install server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let client_body = b"fail-fast client".to_vec();
    let library_body = test_jar(
        "org/example/failfast/FailFastLibrary.class",
        b"fail-fast library",
    );
    let version_body = serde_json::to_vec(&serde_json::json!({
        "id": version_id,
        "downloads": {
            "client": {
                "url": format!("{base_url}/client.jar"),
                "sha1": sha1_hex(&client_body),
                "size": client_body.len()
            }
        },
        "libraries": [{
            "name": "org.example:failfast:1.0.0",
            "downloads": {
                "artifact": {
                    "path": "org/example/failfast/1.0.0/failfast-1.0.0.jar",
                    "url": format!("{base_url}/library.jar"),
                    "sha1": sha1_hex(&library_body),
                    "size": library_body.len()
                }
            }
        }]
    }))
    .expect("serialize fail-fast version");
    let version_sha1 = sha1_hex(&version_body);
    let (blocked_started_tx, blocked_started) = oneshot::channel();
    let (blocked_closed_tx, blocked_closed) = oneshot::channel();
    let blocked_started_tx = Arc::new(Mutex::new(Some(blocked_started_tx)));
    let blocked_closed_tx = Arc::new(Mutex::new(Some(blocked_closed_tx)));
    let blocked_ready = Arc::new(tokio::sync::Notify::new());

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let version_body = version_body.clone();
            let blocked_started_tx = Arc::clone(&blocked_started_tx);
            let blocked_closed_tx = Arc::clone(&blocked_closed_tx);
            let blocked_ready = Arc::clone(&blocked_ready);
            tokio::spawn(async move {
                let Some(path) = read_request_path(&mut socket).await else {
                    return;
                };
                if path == "/version.json" {
                    write_raw_response(&mut socket, "200 OK", &version_body).await;
                } else if path == blocked_path {
                    if let Some(started) = blocked_started_tx.lock().await.take() {
                        let _ = started.send(());
                    }
                    blocked_ready.notify_one();
                    let mut probe = [0_u8; 1];
                    let _ = socket.read(&mut probe).await;
                    if let Some(closed) = blocked_closed_tx.lock().await.take() {
                        let _ = closed.send(());
                    }
                } else if path == failing_path {
                    blocked_ready.notified().await;
                    write_raw_response(&mut socket, "404 Not Found", b"missing").await;
                } else {
                    write_raw_response(&mut socket, "404 Not Found", b"missing").await;
                }
            });
        }
    });

    FailFastInstallServer {
        version_url: format!("{base_url}/version.json"),
        version_sha1,
        blocked_started,
        blocked_closed,
        server,
    }
}

struct NonemptyAssetInstallServer {
    version_url: String,
    version_sha1: String,
    object_base_url: String,
    asset_index_id: String,
    asset_index: Vec<u8>,
    version_json_size: usize,
    object: Vec<u8>,
    object_hash: String,
    distinct: Vec<u8>,
    distinct_hash: String,
    empty_hash: String,
    object_path: String,
    distinct_path: String,
    empty_path: String,
    requests: mpsc::UnboundedReceiver<String>,
}

async fn spawn_nonempty_asset_install_server(version_id: &str) -> NonemptyAssetInstallServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind nonempty asset install server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let client = b"nonempty-asset-client".to_vec();
    let object = b"retained-asset-object".to_vec();
    let distinct = b"distinct-retained-asset-object".to_vec();
    let object_hash = sha1_hex(&object);
    let distinct_hash = sha1_hex(&distinct);
    let empty_hash = sha1_hex(b"");
    let asset_index_id = format!("{version_id}-assets");
    let asset_index = serde_json::json!({
        "virtual": true,
        "objects": {
            "object.bin": { "hash": object_hash, "size": object.len() },
            "duplicate/object.bin": { "hash": object_hash, "size": object.len() },
            "distinct.bin": { "hash": distinct_hash, "size": distinct.len() },
            "empty.bin": { "hash": empty_hash, "size": 0 }
        }
    })
    .to_string()
    .into_bytes();
    let version = serde_json::json!({
        "id": version_id,
        "downloads": { "client": {
            "url": format!("{base_url}/client.jar"),
            "sha1": sha1_hex(&client),
            "size": client.len()
        }},
        "assetIndex": {
            "id": asset_index_id,
            "url": format!("{base_url}/asset-index.json"),
            "sha1": sha1_hex(&asset_index),
            "size": asset_index.len()
        },
        "libraries": []
    })
    .to_string()
    .into_bytes();
    let version_sha1 = sha1_hex(&version);
    let version_json_size = version.len();
    let object_path = format!("/{}/{}", &object_hash[..2], object_hash);
    let distinct_path = format!("/{}/{}", &distinct_hash[..2], distinct_hash);
    let empty_path = format!("/{}/{}", &empty_hash[..2], empty_hash);
    let responses = Arc::new(HashMap::from([
        ("/version.json".to_string(), version),
        ("/client.jar".to_string(), client),
        ("/asset-index.json".to_string(), asset_index.clone()),
        (object_path.clone(), object.clone()),
        (distinct_path.clone(), distinct.clone()),
        (empty_path.clone(), Vec::new()),
    ]));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let responses = Arc::clone(&responses);
            let request_tx = request_tx.clone();
            tokio::spawn(async move {
                let Some(path) = read_request_path(&mut socket).await else {
                    return;
                };
                let _ = request_tx.send(path.clone());
                match responses.get(&path) {
                    Some(body) => write_raw_response(&mut socket, "200 OK", body).await,
                    None => write_raw_response(&mut socket, "404 Not Found", b"not found").await,
                }
            });
        }
    });

    NonemptyAssetInstallServer {
        version_url: format!("{base_url}/version.json"),
        version_sha1,
        object_base_url: base_url,
        asset_index_id,
        asset_index,
        version_json_size,
        object,
        object_hash,
        distinct,
        distinct_hash,
        empty_hash,
        object_path,
        distinct_path,
        empty_path,
        requests: request_rx,
    }
}

struct BlockedAssetInstallServer {
    version_url: String,
    version_sha1: String,
    object_base_url: String,
    asset_index_id: String,
    object_hash: String,
    object_started: oneshot::Receiver<()>,
    object_connection_closed: oneshot::Receiver<()>,
}

async fn spawn_blocked_asset_install_server(version_id: &str) -> BlockedAssetInstallServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blocked asset install server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let client = b"blocked-asset-client".to_vec();
    let object = b"blocked-asset-object".to_vec();
    let object_hash = sha1_hex(&object);
    let object_path = format!("/{}/{}", &object_hash[..2], object_hash);
    let asset_index_id = format!("{version_id}-assets");
    let asset_index = serde_json::json!({
        "objects": {
            "blocked.bin": { "hash": object_hash, "size": object.len() }
        }
    })
    .to_string()
    .into_bytes();
    let version = serde_json::json!({
        "id": version_id,
        "downloads": { "client": {
            "url": format!("{base_url}/client.jar"),
            "sha1": sha1_hex(&client),
            "size": client.len()
        }},
        "assetIndex": {
            "id": asset_index_id,
            "url": format!("{base_url}/asset-index.json"),
            "sha1": sha1_hex(&asset_index),
            "size": asset_index.len()
        },
        "libraries": []
    })
    .to_string()
    .into_bytes();
    let version_sha1 = sha1_hex(&version);
    let responses = Arc::new(HashMap::from([
        ("/version.json".to_string(), version),
        ("/client.jar".to_string(), client),
        ("/asset-index.json".to_string(), asset_index),
    ]));
    let (object_started_tx, object_started_rx) = oneshot::channel();
    let (object_closed_tx, object_closed_rx) = oneshot::channel();
    let object_started_tx = Arc::new(Mutex::new(Some(object_started_tx)));
    let object_closed_tx = Arc::new(Mutex::new(Some(object_closed_tx)));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let responses = Arc::clone(&responses);
            let object_path = object_path.clone();
            let object_started_tx = Arc::clone(&object_started_tx);
            let object_closed_tx = Arc::clone(&object_closed_tx);
            tokio::spawn(async move {
                let Some(path) = read_request_path(&mut socket).await else {
                    return;
                };
                if path == object_path {
                    if let Some(sender) = object_started_tx.lock().await.take() {
                        let _ = sender.send(());
                    }
                    let mut byte = [0_u8; 1];
                    while socket.read(&mut byte).await.is_ok_and(|read| read != 0) {}
                    if let Some(sender) = object_closed_tx.lock().await.take() {
                        let _ = sender.send(());
                    }
                    return;
                }
                match responses.get(&path) {
                    Some(body) => write_raw_response(&mut socket, "200 OK", body).await,
                    None => write_raw_response(&mut socket, "404 Not Found", b"not found").await,
                }
            });
        }
    });

    BlockedAssetInstallServer {
        version_url: format!("{base_url}/version.json"),
        version_sha1,
        object_base_url: base_url,
        asset_index_id,
        object_hash,
        object_started: object_started_rx,
        object_connection_closed: object_closed_rx,
    }
}

async fn spawn_reconstruction_parity_server(
    version_id: &str,
) -> (String, String, mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind reconstruction server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let client = b"reconstruction-client".to_vec();
    let [exact, observed, observed_two] = reconstruction_parity_library_bodies();
    let log_config = b"<log4j/>".to_vec();
    let asset_index = br#"{"objects":{}}"#.to_vec();
    let version = serde_json::json!({
        "id": version_id,
        "downloads": {
            "client": {
                "url": format!("{base_url}/client.jar"),
                "sha1": sha1_hex(&client),
                "size": client.len()
            }
        },
        "assetIndex": {
            "id": "reconstruction-assets",
            "url": format!("{base_url}/asset-index.json"),
            "sha1": sha1_hex(&asset_index),
            "size": 0
        },
        "logging": {
            "client": {
                "argument": "-Dlog4j.configurationFile=${path}",
                "file": {
                    "id": "reconstruction-log.xml",
                    "url": format!("{base_url}/log-config.xml"),
                    "sha1": sha1_hex(&log_config),
                    "size": log_config.len()
                },
                "type": "log4j2-xml"
            }
        },
        "libraries": [
            {
                "name": "org.example:exact:1.0.0",
                "downloads": { "artifact": {
                    "path": "org/example/exact/1.0.0/exact-1.0.0.jar",
                    "url": format!("{base_url}/libraries/exact.jar"),
                    "sha1": sha1_hex(&exact),
                    "size": exact.len()
                }}
            },
            {
                "name": "org.example:observed:1.0.0",
                "downloads": { "artifact": {
                    "path": "org/example/observed/1.0.0/observed-1.0.0.jar",
                    "url": format!("{base_url}/libraries/observed.jar")
                }}
            },
            {
                "name": "org.example:observed-two:1.0.0",
                "downloads": { "artifact": {
                    "path": "org/example/observed-two/1.0.0/observed-two-1.0.0.jar",
                    "url": format!("{base_url}/libraries/observed-two.jar"),
                    "sha1": sha1_hex(&observed_two)
                }}
            }
        ]
    })
    .to_string()
    .into_bytes();
    let version_sha1 = sha1_hex(&version);
    let responses = Arc::new(HashMap::from([
        ("/version.json".to_string(), version),
        ("/client.jar".to_string(), client),
        ("/libraries/exact.jar".to_string(), exact),
        ("/libraries/observed.jar".to_string(), observed),
        ("/libraries/observed-two.jar".to_string(), observed_two),
        ("/asset-index.json".to_string(), asset_index),
        ("/log-config.xml".to_string(), log_config),
    ]));

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let responses = Arc::clone(&responses);
            let request_tx = request_tx.clone();
            tokio::spawn(async move {
                let Some(path) = read_request_path(&mut socket).await else {
                    return;
                };
                let _ = request_tx.send(path.clone());
                match responses.get(&path) {
                    Some(body) => write_raw_response(&mut socket, "200 OK", body).await,
                    None => write_raw_response(&mut socket, "404 Not Found", b"not found").await,
                }
            });
        }
    });

    (format!("{base_url}/version.json"), version_sha1, request_rx)
}

fn reconstruction_parity_library_bodies() -> [Vec<u8>; 3] {
    [
        test_jar(
            "example/ReconstructionExact.class",
            b"reconstruction-exact-library",
        ),
        checksumless_test_jar(),
        checksumless_test_jar(),
    ]
}

async fn spawn_runtime_reconstruction_server(
    version_id: &str,
) -> (
    String,
    String,
    TestRuntimeSourceDescriptor,
    mpsc::UnboundedReceiver<String>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind runtime reconstruction server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let client = b"runtime-reconstruction-client".to_vec();
    let runtime_file = b"java".to_vec();
    let runtime_manifest = serde_json::json!({
        "files": {
            "bin": { "type": "directory" },
            "bin/java": {
                "type": "file",
                "executable": true,
                "downloads": { "raw": {
                    "url": format!("{base_url}/runtime-file"),
                    "sha1": sha1_hex(&runtime_file),
                    "size": runtime_file.len()
                }}
            },
            "lib": { "type": "directory" },
            "lib/data": {
                "type": "file",
                "downloads": { "raw": {
                    "url": format!("{base_url}/runtime-file"),
                    "sha1": sha1_hex(&runtime_file),
                    "size": runtime_file.len()
                }}
            },
            "java-link": { "type": "link", "target": "bin/java" }
        }
    })
    .to_string()
    .into_bytes();
    let version = serde_json::json!({
        "id": version_id,
        "downloads": { "client": {
            "url": format!("{base_url}/client.jar"),
            "sha1": sha1_hex(&client),
            "size": client.len()
        }},
        "javaVersion": { "component": "jre-legacy", "majorVersion": 8 }
    })
    .to_string()
    .into_bytes();
    let version_sha1 = sha1_hex(&version);
    let runtime_source = TestRuntimeSourceDescriptor {
        component: RuntimeId::from("jre-legacy"),
        url: format!("{base_url}/runtime-manifest.json"),
        sha1: sha1_hex(&runtime_manifest),
        size: runtime_manifest.len() as u64,
    };
    let responses = Arc::new(HashMap::from([
        ("/version.json".to_string(), version),
        ("/client.jar".to_string(), client),
        ("/runtime-manifest.json".to_string(), runtime_manifest),
        ("/runtime-file".to_string(), runtime_file),
    ]));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let responses = Arc::clone(&responses);
            let request_tx = request_tx.clone();
            tokio::spawn(async move {
                let Some(path) = read_request_path(&mut socket).await else {
                    return;
                };
                let _ = request_tx.send(path.clone());
                match responses.get(&path) {
                    Some(body) => write_raw_response(&mut socket, "200 OK", body).await,
                    None => write_raw_response(&mut socket, "404 Not Found", b"not found").await,
                }
            });
        }
    });

    (
        format!("{base_url}/version.json"),
        version_sha1,
        runtime_source,
        request_rx,
    )
}

async fn spawn_preflight_failure_server() -> (String, String, mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind preflight server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let client_body = b"client".to_vec();
    let asset_index_body = br#"{"objects":{}}"#.to_vec();
    let version_body = serde_json::json!({
        "id": "preflight",
        "downloads": {
            "client": {
                "url": format!("{base_url}/client.jar"),
                "sha1": sha1_hex(&client_body),
                "size": client_body.len()
            }
        },
        "assetIndex": {
            "id": "preflight-assets",
            "sha1": sha1_hex(&asset_index_body),
            "size": asset_index_body.len(),
            "url": format!("{base_url}/asset-index.json")
        },
        "javaVersion": {
            "component": "jre-legacy",
            "majorVersion": 8
        },
        "libraries": [{
            "name": "org.example:url-less:1.0.0",
            "downloads": {
                "artifact": {
                    "path": "org/example/url-less/1.0.0/url-less-1.0.0.jar",
                    "sha1": "0101010101010101010101010101010101010101",
                    "size": 7,
                    "url": ""
                }
            }
        }]
    })
    .to_string()
    .into_bytes();
    let version_sha1 = sha1_hex(&version_body);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let Some(path) = read_request_path(&mut socket).await else {
                return;
            };
            let _ = request_tx.send(path.clone());
            let body = match path.as_str() {
                "/version.json" => &version_body,
                "/asset-index.json" => &asset_index_body,
                "/client.jar" => &client_body,
                _ => {
                    write_raw_response(&mut socket, "404 Not Found", b"not found").await;
                    continue;
                }
            };
            write_raw_response(&mut socket, "200 OK", body).await;
        }
    });

    (format!("{base_url}/version.json"), version_sha1, request_rx)
}

fn test_manifest_downloader(
    root: &Path,
    version_id: &str,
    version_url: &str,
    version_sha1: &str,
) -> Downloader {
    let manifest = serde_json::json!({
        "latest": { "release": version_id, "snapshot": version_id },
        "versions": [{
            "id": version_id,
            "type": "release",
            "url": version_url,
            "sha1": version_sha1,
            "complianceLevel": 1
        }]
    });
    Downloader::with_test_install_manifest(
        root,
        serde_json::from_value(manifest).expect("valid test install manifest"),
    )
}

fn asset_index_path(root: &Path, asset_index_id: &str) -> PathBuf {
    assets_dir(root)
        .join("indexes")
        .join(format!("{asset_index_id}.json"))
}

fn asset_object_path(root: &Path, hash: &str) -> PathBuf {
    assets_dir(root).join("objects").join(&hash[..2]).join(hash)
}

fn drain_request_paths(requests: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    std::iter::from_fn(|| requests.try_recv().ok()).collect()
}

fn request_count(requests: &[String], path: &str) -> usize {
    requests
        .iter()
        .filter(|request| request.as_str() == path)
        .count()
}

fn empty_test_install_manifest() -> VersionManifest {
    serde_json::from_value(serde_json::json!({
        "latest": { "release": "", "snapshot": "" },
        "versions": []
    }))
    .expect("valid empty test install manifest")
}

async fn read_request_path(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = vec![0_u8; 1024];
    let mut received = Vec::new();
    loop {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        received.extend_from_slice(&buffer[..read]);
        if received.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&received);
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(ToOwned::to_owned)
}

async fn write_raw_response(socket: &mut tokio::net::TcpStream, status: &str, body: &[u8]) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.write_all(body).await;
}

#[tokio::test]
async fn authenticated_source_retry_binds_observed_bytes() {
    let body = br#"{"id":"1.21.1"}"#.to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, requests) = spawn_scripted_download_server(vec![
        ScriptedDownloadResponse::full("503 Service Unavailable", b"temporary".to_vec()),
        ScriptedDownloadResponse::full("200 OK", body.clone()),
    ])
    .await;

    let source = acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
        &build_http_client(Duration::from_secs(5)),
        &url,
        &expected,
        1024,
        "minecraft_version_json_1.21.1",
        &[Duration::ZERO],
    )
    .await
    .expect("retryable provider failure should recover");

    assert_eq!(source.bytes(), body);
    assert_eq!(source.observed_size(), body.len() as u64);
    let expected_sha1: [u8; 20] = Sha1::digest(&body).into();
    assert_eq!(source.observed_sha1(), expected_sha1);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authenticated_source_interrupted_stream_retries() {
    let body = br#"{"objects":{}}"#.to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, requests) = spawn_scripted_download_server(vec![
        ScriptedDownloadResponse::partial("200 OK", body.len(), b"{\"objects".to_vec()),
        ScriptedDownloadResponse::full("200 OK", body.clone()),
    ])
    .await;

    let source = acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
        &build_http_client(Duration::from_secs(5)),
        &url,
        &expected,
        1024,
        "minecraft_asset_index_legacy",
        &[Duration::ZERO],
    )
    .await
    .expect("interrupted source stream should recover");

    assert_eq!(source.bytes(), body);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authenticated_source_rejects_checksum_mismatch() {
    let body = b"wrong source".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(b"right source"));
    let (url, requests) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;

    acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
        &build_http_client(Duration::from_secs(5)),
        &url,
        &expected,
        1024,
        "minecraft_version_json_1.21.1",
        &[Duration::ZERO],
    )
    .await
    .err()
    .expect("checksum mismatch must fail");

    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authenticated_source_rejects_size_mismatch() {
    let body = b"asset index".to_vec();
    let expected = ExpectedIntegrity {
        size: Some(body.len() as u64 + 1),
        sha1: Some(sha1_hex(&body)),
    };
    let (url, requests) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;

    acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
        &build_http_client(Duration::from_secs(5)),
        &url,
        &expected,
        1024,
        "minecraft_asset_index_legacy",
        &[Duration::ZERO],
    )
    .await
    .err()
    .expect("size mismatch must fail");

    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authenticated_source_rejects_oversize_body() {
    let body = b"oversize source".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, requests) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;

    acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
        &build_http_client(Duration::from_secs(5)),
        &url,
        &expected,
        4,
        "minecraft_version_json_1.21.1",
        &[Duration::ZERO],
    )
    .await
    .err()
    .expect("memory bound must fail");

    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authenticated_source_reports_terminal_provider_failure() {
    let expected = ExpectedIntegrity::from_mojang(7, &sha1_hex(b"missing"));
    let (url, requests) = spawn_scripted_download_server(vec![ScriptedDownloadResponse::full(
        "404 Not Found",
        b"missing".to_vec(),
    )])
    .await;

    acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
        &build_http_client(Duration::from_secs(5)),
        &url,
        &expected,
        1024,
        "minecraft_asset_index_legacy",
        &[Duration::ZERO],
    )
    .await
    .err()
    .expect("terminal provider failure must fail");

    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

struct ScriptedDownloadResponse {
    status: &'static str,
    body: Vec<u8>,
    content_length: usize,
}

impl ScriptedDownloadResponse {
    fn full(status: &'static str, body: Vec<u8>) -> Self {
        let content_length = body.len();
        Self {
            status,
            body,
            content_length,
        }
    }

    fn partial(status: &'static str, content_length: usize, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            content_length,
        }
    }
}

async fn spawn_scripted_download_server(
    responses: Vec<ScriptedDownloadResponse>,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted download server");
    let url = format!("http://{}", listener.local_addr().expect("local addr"));
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests_for_server = Arc::clone(&requests);
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            requests_for_server.fetch_add(1, Ordering::SeqCst);
            let _ = read_request_path(&mut socket).await;
            write_scripted_download_response(&mut socket, response).await;
        }
    });
    (url, requests)
}

async fn write_scripted_download_response(
    socket: &mut tokio::net::TcpStream,
    response: ScriptedDownloadResponse,
) {
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status, response.content_length
    );
    let _ = socket.write_all(header.as_bytes()).await;
    let _ = socket.write_all(&response.body).await;
}
