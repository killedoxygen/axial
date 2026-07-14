use super::assets::{
    AssetObjectDownloadJob, copy_virtual_asset_if_missing, copy_virtual_assets,
    missing_asset_object_jobs, repair_virtual_assets_from_index, unique_asset_object_jobs,
    virtual_asset_destination,
};
use super::client::{adaptive_download_concurrency, build_http_client};
use super::facts::{ExecutionDownloadRequest, execution_download_fact};
use super::install::observe_vanilla_publication_lease_wait_for_test;
use super::integrity::{
    download_size_mismatch, existing_asset_object_satisfies, existing_file_satisfies, hash_file,
    observe_hash_file_calls, verify_download_integrity,
};
use super::libraries::{library_jobs_for, library_verification_plans_for};
use super::library_source::{
    LibrarySourcePool, LibrarySourceRequest, acquire_authenticated_library_source,
};
use super::model::{ActualIntegrity, DownloadIntegrityError};
use super::path_safety::{
    bounded_download_file_label, safe_download_target_label, windows_verbatim_path_string,
};
use super::promotion::{
    promotion_backup_path, selected_promotion_temp_path, sweep_stale_promotion_backups,
    sweep_stale_selected_promotion_temps,
};
use super::runtime::{
    RuntimeEnsurePipeline, finish_runtime_pipeline_after_artifacts, runtime_ensure_progress,
};
use super::transfer::{
    AuthenticatedSelectedArtifactSource, ExactPublicationOutcome, PreparedSelectedArtifactInstall,
    SelectedArtifactSourceRequest, SelectedPromotionTestControl, SelectedPromotionTestStage,
    acquire_authenticated_selected_artifact_source,
    acquire_authenticated_selected_artifact_source_with_retry_delays_for_test,
    download_file_with_client, download_file_with_client_report,
    download_file_with_client_report_with_retry_delays, download_temp_path,
    ensure_selected_artifact_with_client, execute_download_to_temp,
    materialize_authenticated_library_source, materialize_authenticated_selected_artifact_source,
    materialize_authenticated_selected_artifact_source_with_control, prepare_library_publication,
    prepare_selected_artifact_install, publish_authenticated_retained_file_for_test,
    remove_stale_download_temp,
};
use super::*;
use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::KnownGoodArtifactKind;
use crate::launch::{JavaVersion, Library, LibraryArtifact, LibraryDownload, maven_to_path};
use crate::managed_fs::ManagedDir;
use crate::managed_publication::ManagedRootPublicationLease;
use crate::manifest::VersionManifest;
use crate::paths::{assets_dir, libraries_dir, versions_dir};
use crate::rules::Environment;
use crate::runtime::{
    RuntimeEnsureEvent, RuntimeId, RuntimeSourceReceipt, TestRuntimeSourceDescriptor,
};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
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
    assert_eq!(events.len(), 1);
    let event = &events[0];
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
async fn reconstruction_matches_install_without_touching_seeded_destinations() {
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
    let install_requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    for path in [
        "/version.json",
        "/client.jar",
        "/libraries/exact.jar",
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
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn reconstruction_derives_runtime_inventory_without_runtime_effects() {
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
async fn install_version_with_facts_emits_private_download_facts_only() {
    let root = temp_dir("install-private-facts");
    let (version_url, version_sha1, _requests, release_library) =
        spawn_overlapped_install_server().await;
    release_library
        .send(())
        .expect("release library response before request");
    let downloader = test_manifest_downloader(&root, "overlap", &version_url, &version_sha1);
    let mut events = Vec::new();
    let mut facts = Vec::new();

    let receipt = downloader
        .install_version_with_facts(
            "overlap",
            |progress| events.push(progress),
            |fact| facts.push(fact),
        )
        .await
        .expect("install should succeed");

    assert_eq!(receipt.version_id(), "overlap");
    assert!(
        !receipt
            .into_activation_source()
            .into_parts()
            .1
            .entries()
            .is_empty()
    );

    assert!(
        events
            .iter()
            .any(|event| event.phase == "done" && event.done)
    );
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::ArtifactMissing)
    );
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::ArtifactVerified)
    );
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
    );
    let progress_json = serde_json::to_string(&events).expect("progress json");
    assert!(!progress_json.contains("facts"));
    assert!(!progress_json.contains("sha1"));
    let version_root = versions_dir(&root).join("overlap");
    let version_json = fs::read(version_root.join("overlap.json")).expect("published version json");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&version_json)
            .expect("published version metadata")["id"]
            .as_str(),
        Some("overlap")
    );
    assert_eq!(
        fs::read(version_root.join("overlap.jar")).expect("published client jar"),
        b"client"
    );
    assert_settled_version_bundle_lane(&root);

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
async fn cancelling_normal_install_while_waiting_for_lease_does_not_detach_publication() {
    let version_id = "normal-lease-wait-cancellation";
    let root = temp_dir(version_id);
    fs::create_dir_all(&root).expect("create normal install root");
    let held_lease = ManagedRootPublicationLease::acquire(
        ManagedDir::open_root(&root).expect("open held normal install root"),
    )
    .await
    .expect("acquire held normal install lease");
    let reached = observe_vanilla_publication_lease_wait_for_test(version_id);
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
        .expect("probe should acquire after cancelled waiter")
        .expect("probe lease task should finish")
        .expect("probe lease should be acquired");

    let version_root = versions_dir(&root).join(version_id);
    assert!(!version_root.join(format!("{version_id}.json")).exists());
    assert!(!version_root.join(format!("{version_id}.jar")).exists());
    assert!(
        !assets_dir(&root)
            .join("log_configs/reconstruction-log.xml")
            .exists()
    );
    assert!(!root.join(".axial-publication/version-bundle").exists());

    drop(probe_lease);
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
async fn selected_missing_artifact_fact_is_emitted_before_download_failure() {
    let root = temp_dir("selected-missing-artifact-fact");
    let destination = root.join("artifact.jar");
    let expected = ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let url = spawn_download_response_server(
        "503 Service Unavailable",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        b"unavailable".to_vec(),
        4,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();

    let result = ensure_selected_artifact_with_client(
        SelectedDownloadArtifactKind::Library,
        &client,
        &url,
        &destination,
        &expected,
        Some(&fact_tx),
    )
    .await;

    assert!(result.is_err());
    drop(fact_tx);
    let mut facts = Vec::new();
    while let Some(fact) = fact_rx.recv().await {
        facts.push(fact);
    }
    assert!(facts.iter().any(|fact| {
        fact.kind == ExecutionDownloadFactKind::ArtifactMissing
            && fact.target == "minecraft_library_artifact"
    }));
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::ProviderFailure)
    );
    assert!(!destination.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn selected_existing_corrupt_artifact_is_replaced_after_verified_download() {
    let root = temp_dir("selected-corrupt-artifact-fact");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    fs::write(&destination, b"wrong").expect("write corrupt artifact");
    let body = b"fresh".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body.clone(),
        1,
    )
    .await;
    let client = build_http_client(Duration::from_secs(1));
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();

    let result = ensure_selected_artifact_with_client(
        SelectedDownloadArtifactKind::Library,
        &client,
        &url,
        &destination,
        &expected,
        Some(&fact_tx),
    )
    .await;

    assert!(result.expect("corrupt artifact should self-heal").is_some());
    assert_eq!(fs::read(&destination).expect("artifact replaced"), body);
    drop(fact_tx);
    let mut facts = Vec::new();
    while let Some(fact) = fact_rx.recv().await {
        facts.push(fact);
    }
    assert!(facts.iter().any(|fact| {
        fact.kind == ExecutionDownloadFactKind::ChecksumMismatch
            && fact.target == "minecraft_library_artifact"
            && fact
                .fields
                .iter()
                .any(|(key, value)| key == "algorithm" && value == "sha1")
    }));
    assert!(facts.iter().any(|fact| {
        fact.kind == ExecutionDownloadFactKind::Promoted
            && fact.target == "minecraft_library_artifact"
            && fact
                .fields
                .iter()
                .any(|(key, value)| key == "replaced" && value == "corrupt")
    }));
    assert!(
        !facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::NetworkFailure)
    );
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn selected_existing_unsupported_artifact_blocks_without_network() {
    let root = temp_dir("selected-unsupported-artifact-fact");
    let destination = root.join("artifact.jar");
    fs::create_dir_all(&destination).expect("create unsupported artifact directory");
    let expected = ExpectedIntegrity::from_mojang(5, &sha1_hex(b"fresh"));
    let client = build_http_client(Duration::from_secs(1));
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();

    let result = ensure_selected_artifact_with_client(
        SelectedDownloadArtifactKind::Library,
        &client,
        "http://127.0.0.1:9/artifact.jar",
        &destination,
        &expected,
        Some(&fact_tx),
    )
    .await;

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
    assert!(destination.is_dir());
    drop(fact_tx);
    let mut facts = Vec::new();
    while let Some(fact) = fact_rx.recv().await {
        facts.push(fact);
    }
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::OwnershipRefused)
    );
    assert!(
        !facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::NetworkFailure)
    );
    assert!(
        !facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::WrittenToTemp)
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_task_is_aborted_when_artifact_install_fails() {
    struct RuntimeGuard(Arc<AtomicBool>);

    impl Drop for RuntimeGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_in_task = Arc::clone(&cancelled);
    let (started_tx, started_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = RuntimeGuard(cancelled_in_task);
        let _ = started_tx.send(());
        std::future::pending::<
            Result<Option<RuntimeSourceReceipt>, crate::runtime::JavaRuntimeLookupError>,
        >()
        .await
    });
    started_rx.await.expect("runtime task should start");
    let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());

    let result = timeout(
        Duration::from_millis(100),
        finish_runtime_pipeline_after_artifacts(
            Some(runtime_pipeline(task)),
            Err::<(), _>(artifact_error),
            &mut |_| {},
        ),
    )
    .await
    .expect("runtime cancellation should settle promptly");

    assert!(matches!(
        result,
        Err(DownloadError::ResolveManifest(message)) if message == "artifact failed"
    ));
    assert!(
        cancelled.load(Ordering::SeqCst),
        "runtime task guard must be dropped before return"
    );
}

#[tokio::test]
async fn runtime_error_is_reported_when_artifact_install_succeeds() {
    let task = tokio::spawn(async {
        Err::<Option<RuntimeSourceReceipt>, _>(crate::runtime::JavaRuntimeLookupError::Download(
            "runtime failed".to_string(),
        ))
    });

    let result =
        finish_runtime_pipeline_after_artifacts(Some(runtime_pipeline(task)), Ok(()), &mut |_| {})
            .await;

    assert!(matches!(
        result,
        Err(DownloadError::PrepareRuntime(message))
            if message == "failed to install java runtime: runtime failed"
    ));
}

#[tokio::test]
async fn artifact_error_is_preserved_when_runtime_also_fails() {
    let task = tokio::spawn(async {
        Err::<Option<RuntimeSourceReceipt>, _>(crate::runtime::JavaRuntimeLookupError::Download(
            "runtime failed".to_string(),
        ))
    });
    let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());

    let result = finish_runtime_pipeline_after_artifacts(
        Some(runtime_pipeline(task)),
        Err::<(), _>(artifact_error),
        &mut |_| {},
    )
    .await;

    assert!(matches!(
        result,
        Err(DownloadError::ResolveManifest(message)) if message == "artifact failed"
    ));
}

fn runtime_pipeline(
    task: tokio::task::JoinHandle<
        Result<Option<RuntimeSourceReceipt>, crate::runtime::JavaRuntimeLookupError>,
    >,
) -> RuntimeEnsurePipeline {
    let (_progress_tx, progress_rx) = mpsc::unbounded_channel();
    RuntimeEnsurePipeline { task, progress_rx }
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
    let mc_dir = Path::new("/tmp/axial-test");
    let libraries = vec![
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-arm64"),
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-x86"),
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows"),
    ];

    let jobs = strict_library_jobs(mc_dir, &libraries, &env);
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
    let job = strict_library_jobs(Path::new("/tmp/axial-test"), &[lib], &env)
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
fn library_jobs_deduplicate_same_destination() {
    let env = Environment {
        os_name: "linux".to_string(),
        os_arch: "x86_64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };
    let mc_dir = Path::new("/tmp/axial-test");
    let libraries = vec![
        normal_library("org.example:duplicate:1.0.0"),
        normal_library("org.example:duplicate:1.0.0"),
    ];

    let jobs = strict_library_jobs(mc_dir, &libraries, &env);

    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].name.contains("duplicate-1.0.0.jar"));
}

#[test]
fn library_jobs_reject_conflicting_destination_contracts() {
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

    let error = library_jobs_for(Path::new("/tmp/axial-test"), &[first, second], &env)
        .expect_err("conflicting plan");

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

    let error =
        library_jobs_for(Path::new("/tmp/axial-test"), &[lib], &env).expect_err("missing source");
    assert_eq!(error, LibraryPlanError::MissingDownloadSource);
}

#[test]
fn library_planning_rejects_invalid_nonempty_checksum() {
    let mut direct = normal_library("org.example:invalid-direct-checksum:1.0.0");
    direct.sha1 = "not-a-sha1".to_string();
    let mut legacy = normal_library("org.example:invalid-legacy-checksum:1.0.0");
    legacy.checksums = vec!["not-a-sha1".to_string()];

    for lib in [direct, legacy] {
        let error = library_jobs_for(
            Path::new("/tmp/axial-test"),
            &[lib],
            &crate::rules::default_environment(),
        )
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

    let jobs = strict_library_jobs(Path::new("/tmp/axial-test"), &[lib], &env);

    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].name.ends_with("arm64.jar"));
}

#[test]
fn unique_asset_object_jobs_deduplicate_same_hash() {
    let objects_dir = Path::new("/tmp/axial-test/assets/objects");
    let hash_a = "abcdef1234567890abcdef1234567890abcdef12";
    let hash_b = "1234567890abcdef1234567890abcdef12345678";

    let jobs = unique_asset_object_jobs(objects_dir, [(hash_a, 4), (hash_a, 4), (hash_b, 8)])
        .expect("valid asset jobs");

    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].hash, hash_a);
    assert_eq!(jobs[0].path, objects_dir.join("ab").join(hash_a));
    assert_eq!(jobs[0].expected, ExpectedIntegrity::from_mojang(4, hash_a));
    assert_eq!(jobs[1].hash, hash_b);
    assert_eq!(jobs[1].path, objects_dir.join("12").join(hash_b));
    assert_eq!(jobs[1].expected, ExpectedIntegrity::from_mojang(8, hash_b));
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
        Path::new("/tmp/axial-test/assets/objects"),
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
    let result =
        unique_asset_object_jobs(Path::new("/tmp/axial-test/assets/objects"), [(hash, -1)]);

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[test]
fn unique_asset_object_jobs_rejects_one_character_hash() {
    let objects_dir = Path::new("/tmp/axial-test/assets/objects");
    let result = unique_asset_object_jobs(objects_dir, [("a", 4)]);

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[test]
fn unique_asset_object_jobs_rejects_non_hex_hash() {
    let objects_dir = Path::new("/tmp/axial-test/assets/objects");
    let result = unique_asset_object_jobs(
        objects_dir,
        [("abcdef1234567890abcdef1234567890abcdef1z", 4)],
    );

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[tokio::test]
async fn missing_asset_object_jobs_uses_content_addressed_size_fast_path() {
    let root = temp_dir("asset-filter");
    let objects_dir = root.join("assets").join("objects");
    let existing_hash = sha1_hex(b"asset");
    let missing_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let wrong_size_hash = "cccccccccccccccccccccccccccccccccccccccc";
    let wrong_hash_same_size = "dddddddddddddddddddddddddddddddddddddddd";
    let existing_path = objects_dir.join(&existing_hash[..2]).join(&existing_hash);
    let missing_path = objects_dir.join("bb").join(missing_hash);
    let wrong_size_path = objects_dir.join("cc").join(wrong_size_hash);
    let wrong_hash_same_size_path = objects_dir.join("dd").join(wrong_hash_same_size);
    fs::create_dir_all(existing_path.parent().expect("existing parent"))
        .expect("create existing parent");
    fs::create_dir_all(wrong_size_path.parent().expect("wrong size parent"))
        .expect("create wrong size parent");
    fs::create_dir_all(
        wrong_hash_same_size_path
            .parent()
            .expect("wrong hash parent"),
    )
    .expect("create wrong hash parent");
    fs::write(&existing_path, b"asset").expect("write existing asset");
    fs::write(&wrong_size_path, b"short").expect("write wrong size asset");
    fs::write(&wrong_hash_same_size_path, b"asset").expect("write wrong hash asset");

    let jobs = missing_asset_object_jobs(vec![
        AssetObjectDownloadJob {
            hash: existing_hash.clone(),
            path: existing_path,
            expected: ExpectedIntegrity::from_mojang(5, &existing_hash),
        },
        AssetObjectDownloadJob {
            hash: missing_hash.to_string(),
            path: missing_path.clone(),
            expected: ExpectedIntegrity::from_mojang(5, missing_hash),
        },
        AssetObjectDownloadJob {
            hash: wrong_size_hash.to_string(),
            path: wrong_size_path.clone(),
            expected: ExpectedIntegrity::from_mojang(6, wrong_size_hash),
        },
        AssetObjectDownloadJob {
            hash: wrong_hash_same_size.to_string(),
            path: wrong_hash_same_size_path.clone(),
            expected: ExpectedIntegrity::from_mojang(5, wrong_hash_same_size),
        },
    ])
    .await
    .expect("filter jobs");

    let paths = jobs.into_iter().map(|job| job.path).collect::<HashSet<_>>();

    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&missing_path));
    assert!(paths.contains(&wrong_size_path));
    assert!(!paths.contains(&wrong_hash_same_size_path));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn content_addressed_asset_object_satisfies_without_hashing() {
    let root = temp_dir("asset-fast-path");
    let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let path = root.join("assets").join("objects").join("aa").join(hash);
    fs::create_dir_all(path.parent().expect("asset parent")).expect("create asset parent");
    fs::write(&path, b"wrong").expect("write same-size wrong asset");
    let observer = observe_hash_file_calls(&path);

    assert!(
        existing_asset_object_satisfies(&path, &ExpectedIntegrity::from_mojang(5, hash))
            .await
            .expect("asset object fast path")
    );
    assert_eq!(
        observer.calls(),
        0,
        "content-addressed asset ensure should not rehash existing objects"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn existing_file_satisfies_rejects_size_and_sha1_mismatch() {
    let root = temp_dir("existing-integrity");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join("artifact.jar");
    fs::write(&path, b"artifact").expect("write artifact");
    let good_sha1 = sha1_hex(b"artifact");

    assert!(
        existing_file_satisfies(&path, &ExpectedIntegrity::from_mojang(8, &good_sha1))
            .await
            .expect("matching file")
    );
    assert!(
        !existing_file_satisfies(&path, &ExpectedIntegrity::from_mojang(7, &good_sha1))
            .await
            .expect("size mismatch")
    );
    assert!(
        !existing_file_satisfies(
            &path,
            &ExpectedIntegrity::from_mojang(8, "0000000000000000000000000000000000000000")
        )
        .await
        .expect("sha1 mismatch")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn download_file_with_client_rejects_oversized_content_length_before_temp_file() {
    let root = temp_dir("oversized-content-length");
    let destination = root.join("nested").join("artifact.jar");
    let tmp_path = download_temp_path(&destination);
    let expected = ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let url = spawn_download_response_server(
        "200 OK",
        vec![
            (
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            ),
            ("Content-Length".to_string(), "9".to_string()),
        ],
        b"short".to_vec(),
        3,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let result = download_file_with_client(&client, &url, &destination, &expected).await;

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
    assert!(!tmp_path.exists());
    assert!(!destination.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn download_file_with_client_rejects_stream_past_expected_size_and_cleans_temp() {
    let root = temp_dir("oversized-stream");
    let destination = root.join("nested").join("artifact.jar");
    let tmp_path = download_temp_path(&destination);
    let expected = ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        b"123456789".to_vec(),
        3,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let result = download_file_with_client(&client, &url, &destination, &expected).await;

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
    assert!(!tmp_path.exists());
    assert!(!destination.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn download_file_with_client_rejects_streamed_sha1_mismatch_and_cleans_temp() {
    let root = temp_dir("sha1-stream-mismatch");
    let destination = root.join("nested").join("artifact.jar");
    let tmp_path = download_temp_path(&destination);
    let expected = ExpectedIntegrity::from_mojang(8, "0000000000000000000000000000000000000000");
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        b"artifact".to_vec(),
        3,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let result = download_file_with_client(&client, &url, &destination, &expected).await;

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
    assert!(!tmp_path.exists());
    assert!(!destination.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn execute_download_to_temp_reports_successful_integrity() {
    let root = temp_dir("execution-download-success");
    let destination = root.join("artifact.jar");
    let body = b"artifact".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body.clone(),
        1,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let report = execute_download_to_temp(
        &client,
        ExecutionDownloadRequest::launcher_managed(&url, &destination, &expected),
    )
    .await
    .expect("execute download");

    assert_eq!(report.bytes_written, body.len() as u64);
    assert!(
        report
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::WrittenToTemp)
    );
    assert!(
        report
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::ArtifactVerified)
    );
    assert!(
        report
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
    );
    assert_eq!(
        fs::read(&destination).expect("read promoted artifact"),
        body
    );
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
}

fn checksumless_test_jar() -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    archive
        .start_file(
            "example/Entry.class",
            zip::write::SimpleFileOptions::default(),
        )
        .expect("start jar entry");
    std::io::Write::write_all(&mut archive, b"entry").expect("write jar entry");
    archive.finish().expect("finish jar").into_inner()
}

#[tokio::test]
async fn execute_download_to_temp_reports_missing_metadata_without_promoting() {
    let root = temp_dir("execution-download-missing-metadata");
    let destination = root.join("artifact.jar");
    let body = b"artifact".to_vec();
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body.clone(),
        1,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let error = execute_download_to_temp(
        &client,
        ExecutionDownloadRequest::launcher_managed(
            &url,
            &destination,
            &ExpectedIntegrity::default(),
        ),
    )
    .await
    .expect_err("metadata-free download should fail closed");

    assert_eq!(error.kind, ExecutionDownloadFactKind::MetadataMissing);
    assert!(
        error
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::MetadataMissing)
    );
    assert!(
        !error
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::ArtifactVerified)
    );
    assert!(!destination.exists());
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn execute_download_to_temp_reports_invalid_metadata_without_promoting() {
    let root = temp_dir("execution-download-invalid-metadata");
    let destination = root.join("artifact.jar");
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        b"artifact".to_vec(),
        0,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));
    let expected = ExpectedIntegrity::from_sha1("not-a-sha1");

    let error = execute_download_to_temp(
        &client,
        ExecutionDownloadRequest::launcher_managed(&url, &destination, &expected),
    )
    .await
    .expect_err("invalid metadata should fail before download");

    assert_eq!(error.kind, ExecutionDownloadFactKind::MetadataInvalid);
    assert!(
        error
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::MetadataInvalid)
    );
    assert!(!destination.exists());
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn download_file_with_client_report_preserves_redacted_failure_facts() {
    let root = temp_dir("download-report-invalid-metadata");
    let destination = root.join("nested").join("artifact.jar");
    let expected = ExpectedIntegrity::from_sha1("not-a-sha1");

    let error = download_file_with_client_report(
        &reqwest::Client::new(),
        "https://example.invalid/artifact.jar?token=secret",
        &destination,
        &expected,
    )
    .await
    .expect_err("invalid metadata should fail with report");

    assert_eq!(error.kind, ExecutionDownloadFactKind::MetadataInvalid);
    assert!(
        error
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::MetadataInvalid)
    );
    let facts_json = serde_json::to_string(&error.facts).expect("facts json");
    assert!(!facts_json.contains(root.to_string_lossy().as_ref()));
    assert!(!facts_json.contains("example.invalid"));
    assert!(!facts_json.contains("token"));
    assert!(!facts_json.contains("secret"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn download_file_with_client_report_discards_stale_temp_before_promotion() {
    let root = temp_dir("download-report-stale-temp");
    let destination = root.join("nested").join("artifact.jar");
    let temp_path = download_temp_path(&destination);
    fs::create_dir_all(destination.parent().expect("destination parent"))
        .expect("create destination parent");
    fs::write(&temp_path, b"partial bytes from interrupted worker").expect("write stale temp");
    let body = b"fresh launcher managed artifact".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body.clone(),
        1,
    )
    .await;

    let report = download_file_with_client_report(
        &build_http_client(Duration::from_secs(5)),
        &url,
        &destination,
        &expected,
    )
    .await
    .expect("stale temp should be discarded before promotion");

    for expected_kind in [
        ExecutionDownloadFactKind::TempDiscarded,
        ExecutionDownloadFactKind::WrittenToTemp,
        ExecutionDownloadFactKind::Promoted,
    ] {
        assert!(report.facts.iter().any(|fact| fact.kind == expected_kind));
    }
    assert_eq!(fs::read(&destination).expect("promoted artifact"), body);
    assert!(!temp_path.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn execute_download_to_temp_reports_provider_failure_fact() {
    let root = temp_dir("execution-download-provider-failure");
    let destination = root.join("artifact.jar");
    let url = spawn_download_response_server(
        "503 Service Unavailable",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        b"unavailable".to_vec(),
        1,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));
    let expected = ExpectedIntegrity::from_mojang(12, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    let error = execute_download_to_temp(
        &client,
        ExecutionDownloadRequest::launcher_managed(&url, &destination, &expected),
    )
    .await
    .expect_err("provider failure should not promote");

    assert_eq!(error.kind, ExecutionDownloadFactKind::ProviderFailure);
    assert!(error.facts.iter().any(|fact| {
        fact.kind == ExecutionDownloadFactKind::ProviderFailure
            && fact
                .fields
                .iter()
                .any(|(key, value)| key == "status" && value == "503")
    }));
    assert!(!destination.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn execute_download_to_temp_reports_interrupted_short_response_without_promoting() {
    let root = temp_dir("execution-download-interrupted");
    let destination = root.join("artifact.jar");
    let url = spawn_download_response_server(
        "200 OK",
        vec![
            (
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            ),
            ("Content-Length".to_string(), "12".to_string()),
        ],
        b"partial".to_vec(),
        1,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));
    let expected = ExpectedIntegrity::from_mojang(12, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    let error = execute_download_to_temp(
        &client,
        ExecutionDownloadRequest::launcher_managed(&url, &destination, &expected),
    )
    .await
    .expect_err("short response should not promote");

    assert!(
        error
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::Interrupted)
    );
    assert!(
        error
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::TempDiscarded)
    );
    assert!(!destination.exists());
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn execute_download_to_temp_refuses_non_launcher_owned_targets() {
    let root = temp_dir("execution-download-ownership");
    let destination = root.join("artifact.jar");
    let client = build_http_client(Duration::from_secs(5));
    let expected = ExpectedIntegrity::default();

    for ownership in [
        ExecutionDownloadOwnership::UserOwned,
        ExecutionDownloadOwnership::Unknown,
    ] {
        let error = execute_download_to_temp(
            &client,
            ExecutionDownloadRequest {
                url: "http://127.0.0.1:9/artifact.jar",
                destination: &destination,
                expected: &expected,
                ownership,
            },
        )
        .await
        .expect_err("non-launcher ownership should be refused before network");

        assert_eq!(error.kind, ExecutionDownloadFactKind::OwnershipRefused);
        assert!(
            error
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionDownloadFactKind::OwnershipRefused)
        );
        assert!(!destination.exists());
        assert!(!download_temp_path(&destination).exists());
    }
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
    let expected = ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    assert!(
        std::mem::size_of_val(&hash_file(path)) < 4096,
        "hash_file future should not embed the hash buffer on the task stack"
    );
    assert!(
        std::mem::size_of_val(&existing_file_satisfies(path, &expected)) < 4096,
        "existing-file integrity future should stay small"
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
async fn execute_download_to_temp_replaces_existing_destination() {
    let root = temp_dir("promote-replace");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    fs::write(&destination, b"stale").expect("write stale artifact");
    let body = b"fresh".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body.clone(),
        1,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));

    execute_download_to_temp(
        &client,
        ExecutionDownloadRequest::launcher_managed(&url, &destination, &expected),
    )
    .await
    .expect("execute download");

    assert_eq!(
        fs::read(&destination).expect("read promoted artifact"),
        b"fresh"
    );
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
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
async fn selected_temp_sweep_removes_only_strict_stale_owner_names() {
    let root = temp_dir("selected-temp-owner-sweep");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    let stale_pid = unused_pid_for_test(&[std::process::id()]);
    let stale = root.join(format!("asset-index.json.axial-selected-tmp-{stale_pid}"));
    let current = selected_promotion_temp_path(&destination);
    let malformed = root.join(format!(
        "asset-index.json.axial-selected-tmp-{stale_pid}-extra"
    ));
    let foreign = root.join(format!("other.json.axial-selected-tmp-{stale_pid}"));
    let mut child = spawn_promotion_sweep_child_process();
    let live = root.join(format!(
        "asset-index.json.axial-selected-tmp-{}",
        child.id()
    ));
    for path in [&stale, &current, &malformed, &foreign, &live] {
        fs::write(path, b"reserved").expect("write selected temp fixture");
    }

    let sweep = sweep_stale_selected_promotion_temps(&destination).await;
    let _ = child.kill();
    let _ = child.wait();
    sweep.expect("sweep selected temps");

    assert!(!stale.exists());
    assert!(current.exists());
    assert!(malformed.exists());
    assert!(foreign.exists());
    assert!(live.exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn promote_sweeps_stale_backups_before_replace() {
    let root = temp_dir("promote-sweeps-before-replace");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    let temp_path = download_temp_path(&destination);
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
async fn execute_download_to_temp_removes_temp_when_promotion_fails() {
    let root = temp_dir("promote-cleanup");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    fs::create_dir_all(&destination).expect("create destination directory");
    let body = b"fresh".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let url = spawn_download_response_server(
        "200 OK",
        vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body,
        1,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let result = execute_download_to_temp(
        &client,
        ExecutionDownloadRequest::launcher_managed(&url, &destination, &expected),
    )
    .await;

    let error = result.expect_err("directory destination should fail promotion");
    assert!(
        error
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::PromoteFailed)
    );
    assert!(!download_temp_path(&destination).exists());
    assert!(destination.is_dir());

    let _ = fs::remove_dir_all(root);
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
fn verify_download_integrity_rejects_mismatches() {
    let path = Path::new("/tmp/axial-test/artifact.jar");
    let expected = ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let wrong_size = ActualIntegrity {
        size: 7,
        sha1: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
    };
    let wrong_sha1 = ActualIntegrity {
        size: 8,
        sha1: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
    };

    assert!(matches!(
        verify_download_integrity(path, &expected, &wrong_size),
        Err(DownloadIntegrityError::SizeMismatch { .. })
    ));
    assert!(matches!(
        verify_download_integrity(path, &expected, &wrong_sha1),
        Err(DownloadIntegrityError::Sha1Mismatch { .. })
    ));
}

#[test]
fn download_integrity_errors_report_file_name_without_local_path() {
    let path = Path::new("/home/alice/.minecraft/libraries/org/example/lib.jar");
    let expected = ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let wrong_size = ActualIntegrity {
        size: 7,
        sha1: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
    };

    let message = verify_download_integrity(path, &expected, &wrong_size)
        .expect_err("expected size mismatch")
        .to_string();
    let early_size_message = download_size_mismatch(path, 8, 9).to_string();

    for message in [message, early_size_message] {
        assert!(message.contains("lib.jar"));
        assert!(!message.contains("/home/alice"));
        assert!(!message.contains(".minecraft"));
    }
}

#[test]
fn download_integrity_file_label_falls_back_to_generic_artifact() {
    assert_eq!(bounded_download_file_label(Path::new("/")), "artifact");
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

    let job = strict_library_jobs(
        Path::new("/tmp/axial-test"),
        &[lib],
        &crate::rules::default_environment(),
    )
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

    let job = strict_library_jobs(
        Path::new("/tmp/axial-test"),
        &[lib],
        &crate::rules::default_environment(),
    )
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
    let job = strict_library_jobs(Path::new("/tmp/axial-test"), &[lib], &env)
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

    let job = strict_library_jobs(
        Path::new("/tmp/axial-test"),
        &[lib],
        &crate::rules::default_environment(),
    )
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
        "a".repeat(crate::artifact_path::MAX_ARTIFACT_PATH_SEGMENT_BYTES - ".json".len() + 1);

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

fn strict_library_jobs(
    mc_dir: &Path,
    libraries: &[Library],
    env: &Environment,
) -> Vec<DownloadJob> {
    library_jobs_for(mc_dir, libraries, env).expect("valid library plan")
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

fn libraries_lane_is_settled(root: &Path) -> bool {
    let lane = root.join(".axial-publication/libraries");
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
    let library_body = b"library".to_vec();
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

async fn spawn_reconstruction_parity_server(
    version_id: &str,
) -> (String, String, mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind reconstruction server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let client = b"reconstruction-client".to_vec();
    let exact = b"reconstruction-exact-library".to_vec();
    let observed = checksumless_test_jar();
    let observed_two = checksumless_test_jar();
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
                    "url": format!("{base_url}/libraries/observed.jar"),
                    "sha1": sha1_hex(&observed)
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
async fn download_file_with_client_report_retries_retryable_provider_failure() {
    let root = temp_dir("retry-provider-then-success");
    let destination = root.join("artifact.jar");
    let body = b"artifact".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, requests) = spawn_scripted_download_server(vec![
        ScriptedDownloadResponse::full("500 Internal Server Error", b"temporary".to_vec()),
        ScriptedDownloadResponse::full("200 OK", body.clone()),
    ])
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let report = download_file_with_client_report_with_retry_delays(
        &client,
        &url,
        &destination,
        &expected,
        &[Duration::ZERO],
    )
    .await
    .expect("retryable provider failure should recover");

    assert_eq!(report.bytes_written, body.len() as u64);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(fs::read(&destination).expect("read artifact"), body);
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn download_file_with_client_report_does_not_retry_checksum_mismatch() {
    let root = temp_dir("retry-checksum-mismatch");
    let destination = root.join("artifact.jar");
    let body = b"artifact".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, requests) = spawn_scripted_download_server(vec![
        ScriptedDownloadResponse::full("200 OK", b"wrong-by".to_vec()),
        ScriptedDownloadResponse::full("200 OK", body),
    ])
    .await;
    let client = build_http_client(Duration::from_secs(5));

    let error = download_file_with_client_report_with_retry_delays(
        &client,
        &url,
        &destination,
        &expected,
        &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
    )
    .await
    .expect_err("checksum mismatch should fail without retry");

    assert_eq!(error.kind, ExecutionDownloadFactKind::ChecksumMismatch);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(!destination.exists());
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn download_file_with_client_report_retries_interrupted_body_stream() {
    let root = temp_dir("retry-interrupted-stream");
    let destination = root.join("artifact.jar");
    let body = b"artifact".to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, requests) = spawn_scripted_download_server(vec![
        ScriptedDownloadResponse::partial("200 OK", body.len(), b"art".to_vec()),
        ScriptedDownloadResponse::full("200 OK", body.clone()),
    ])
    .await;
    let client = build_http_client(Duration::from_secs(5));

    download_file_with_client_report_with_retry_delays(
        &client,
        &url,
        &destination,
        &expected,
        &[Duration::ZERO],
    )
    .await
    .expect("interrupted stream should recover");

    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(fs::read(&destination).expect("read artifact"), body);
    assert!(!download_temp_path(&destination).exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_retry_preserves_destination_and_binds_observed_bytes() {
    let root = temp_dir("verified-source-retry-provider");
    let destination = root.join("asset-index.json");
    let temp_path = download_temp_path(&destination);
    fs::create_dir_all(&root).expect("source sentinel root");
    fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
    fs::write(&temp_path, b"temp-sentinel").expect("temp sentinel");
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
    assert_eq!(
        fs::read(&destination).expect("destination sentinel"),
        b"installed-sentinel"
    );
    assert_eq!(
        fs::read(&temp_path).expect("temp sentinel"),
        b"temp-sentinel"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_interrupted_retry_never_mutates_destination() {
    let root = temp_dir("verified-source-retry-interrupted");
    let destination = root.join("asset-index.json");
    let temp_path = download_temp_path(&destination);
    fs::create_dir_all(&root).expect("source sentinel root");
    fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
    fs::write(&temp_path, b"temp-sentinel").expect("temp sentinel");
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
    assert_eq!(
        fs::read(&destination).expect("destination sentinel"),
        b"installed-sentinel"
    );
    assert_eq!(
        fs::read(&temp_path).expect("temp sentinel"),
        b"temp-sentinel"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_checksum_failure_never_mutates_destination() {
    let root = temp_dir("verified-source-checksum-failure");
    let destination = root.join("asset-index.json");
    let temp_path = download_temp_path(&destination);
    fs::create_dir_all(&root).expect("source sentinel root");
    fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
    fs::write(&temp_path, b"temp-sentinel").expect("temp sentinel");
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
    assert_eq!(
        fs::read(&destination).expect("destination sentinel"),
        b"installed-sentinel"
    );
    assert_eq!(
        fs::read(&temp_path).expect("temp sentinel"),
        b"temp-sentinel"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_size_failure_never_mutates_destination() {
    let root = temp_dir("verified-source-size-failure");
    let destination = root.join("asset-index.json");
    let temp_path = download_temp_path(&destination);
    fs::create_dir_all(&root).expect("source sentinel root");
    fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
    fs::write(&temp_path, b"temp-sentinel").expect("temp sentinel");
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
    assert_eq!(
        fs::read(&destination).expect("destination sentinel"),
        b"installed-sentinel"
    );
    assert_eq!(
        fs::read(&temp_path).expect("temp sentinel"),
        b"temp-sentinel"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_oversize_failure_never_mutates_destination() {
    let root = temp_dir("verified-source-oversize-failure");
    let destination = root.join("asset-index.json");
    let temp_path = download_temp_path(&destination);
    fs::create_dir_all(&root).expect("source sentinel root");
    fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
    fs::write(&temp_path, b"temp-sentinel").expect("temp sentinel");
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
    assert_eq!(
        fs::read(&destination).expect("destination sentinel"),
        b"installed-sentinel"
    );
    assert_eq!(
        fs::read(&temp_path).expect("temp sentinel"),
        b"temp-sentinel"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_provider_failure_never_mutates_destination() {
    let root = temp_dir("verified-source-provider-failure");
    let destination = root.join("asset-index.json");
    let temp_path = download_temp_path(&destination);
    fs::create_dir_all(&root).expect("source sentinel root");
    fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
    fs::write(&temp_path, b"temp-sentinel").expect("temp sentinel");
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
    assert_eq!(
        fs::read(&destination).expect("destination sentinel"),
        b"installed-sentinel"
    );
    assert_eq!(
        fs::read(&temp_path).expect("temp sentinel"),
        b"temp-sentinel"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_materialization_consumes_matching_prepared_contract() {
    let root = temp_dir("verified-source-materialization-contract");
    let destination = root.join("asset-index.json");
    let body = br#"{"objects":{}}"#.to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, _) = spawn_scripted_download_server(vec![ScriptedDownloadResponse::full(
        "200 OK",
        body.clone(),
    )])
    .await;
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let prepared = prepare_selected_artifact_install(
        SelectedDownloadArtifactKind::AssetIndex,
        &destination,
        &url,
        "fixture-assets",
        &expected,
        Some(&fact_tx),
    )
    .await
    .expect("prepare destination capability");
    let source = acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
        client: &build_http_client(Duration::from_secs(5)),
        kind: SelectedDownloadArtifactKind::AssetIndex,
        url: &url,
        logical_identity: "fixture-assets",
        expected: &expected,
        max_bytes: 1024,
        target: prepared.target(),
        fact_tx: Some(&fact_tx),
    })
    .await
    .expect("acquire source");

    let materialized =
        materialize_authenticated_selected_artifact_source(prepared, source, Some(&fact_tx))
            .await
            .expect("materialize matching source");

    assert_eq!(materialized.bytes(), body);
    assert_eq!(fs::read(&destination).expect("materialized source"), body);
    let facts = std::iter::from_fn(|| fact_rx.try_recv().ok()).collect::<Vec<_>>();
    let promoted = facts
        .iter()
        .position(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
        .expect("promoted fact");
    let verified = facts
        .iter()
        .position(|fact| fact.kind == ExecutionDownloadFactKind::ArtifactVerified)
        .expect("verified fact");
    assert!(promoted < verified);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_repeat_exact_materialization_reuses_with_retained_backup() {
    let root = temp_dir("selected-repeat-exact");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    materialize_authenticated_selected_artifact_source(prepared, source, None)
        .await
        .expect("first exact transition");
    let backups = selected_reserved_backups(&destination);
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read(&backups[0]).expect("retained backup"),
        b"old-source"
    );

    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    materialize_authenticated_selected_artifact_source(prepared, source, None)
        .await
        .expect("repeat exact materialization");

    assert_eq!(
        fs::read(&destination).expect("exact destination"),
        b"new-source"
    );
    assert!(!selected_promotion_temp_path(&destination).exists());
    assert_eq!(selected_reserved_backups(&destination), backups);

    let (prepared, source) = selected_materialization_fixture(&destination, b"third-source").await;
    materialize_authenticated_selected_artifact_source(prepared, source, None)
        .await
        .err()
        .expect("different replacement must respect retained backup slot");
    assert_eq!(
        fs::read(&destination).expect("bounded destination"),
        b"new-source"
    );
    assert_eq!(selected_reserved_backups(&destination), backups);
    assert_eq!(
        fs::read(selected_promotion_temp_path(&destination)).expect("one retained temp obligation"),
        b"third-source"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_rejects_mismatched_prepared_contract_without_mutation() {
    let root = temp_dir("verified-source-mismatched-contract");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source sentinel root");
    fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
    let body = br#"{"objects":{}}"#.to_vec();
    let source_expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let prepared_expected = ExpectedIntegrity::from_mojang(
        body.len() as i64,
        &sha1_hex(br#"{"objects":{"other":{}}}"#),
    );
    let (url, _) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;
    let prepared = prepare_selected_artifact_install(
        SelectedDownloadArtifactKind::AssetIndex,
        &destination,
        &url,
        "asset-index",
        &prepared_expected,
        None,
    )
    .await
    .expect("prepare destination capability");
    let source = acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
        client: &build_http_client(Duration::from_secs(5)),
        kind: SelectedDownloadArtifactKind::AssetIndex,
        url: &url,
        logical_identity: "asset-index",
        expected: &source_expected,
        max_bytes: 1024,
        target: prepared.target(),
        fact_tx: None,
    })
    .await
    .expect("acquire source");

    materialize_authenticated_selected_artifact_source(prepared, source, None)
        .await
        .err()
        .expect("mismatched contract must fail");

    assert_eq!(
        fs::read(&destination).expect("destination sentinel"),
        b"installed-sentinel"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_rejects_each_cross_identity_recombination_axis() {
    for case in ["kind", "provider", "logical_identity"] {
        let root = temp_dir(&format!("verified-source-cross-{case}"));
        let destination = root.join("artifact.json");
        fs::create_dir_all(&root).expect("source sentinel root");
        fs::write(&destination, b"installed-sentinel").expect("destination sentinel");
        let body = br#"{"shared":"digest"}"#.to_vec();
        let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
        let (source_url, _) =
            spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)])
                .await;
        let source =
            acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                client: &build_http_client(Duration::from_secs(5)),
                kind: SelectedDownloadArtifactKind::AssetIndex,
                url: &source_url,
                logical_identity: "shared-identity",
                expected: &expected,
                max_bytes: 1024,
                target: "asset_index_source",
                fact_tx: None,
            })
            .await
            .expect("acquire authenticated source");
        let prepared_kind = if case == "kind" {
            SelectedDownloadArtifactKind::Library
        } else {
            SelectedDownloadArtifactKind::AssetIndex
        };
        let prepared_provider = if case == "provider" {
            format!("{source_url}?different-provider")
        } else {
            source_url.clone()
        };
        let prepared_identity = if case == "logical_identity" {
            "different-identity"
        } else {
            "shared-identity"
        };
        let prepared = prepare_selected_artifact_install(
            prepared_kind,
            &destination,
            &prepared_provider,
            prepared_identity,
            &expected,
            None,
        )
        .await
        .expect("prepare distinct destination contract");

        assert!(
            matches!(
                materialize_authenticated_selected_artifact_source(prepared, source, None).await,
                Err(DownloadError::Integrity(_))
            ),
            "one mismatched identity axis must reject equal-digest recombination"
        );
        assert_eq!(
            fs::read(&destination).expect("destination sentinel"),
            b"installed-sentinel",
            "{case} mismatch must not mutate destination"
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn authenticated_source_materialization_failure_emits_no_verified_fact() {
    let root = temp_dir("verified-source-materialization-failure");
    fs::create_dir_all(&root).expect("source root");
    let blocked_parent = root.join("blocked-parent");
    fs::write(&blocked_parent, b"not-a-directory").expect("blocked parent");
    let destination = blocked_parent.join("asset-index.json");
    let body = br#"{"objects":{}}"#.to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, _) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let prepared = prepare_selected_artifact_install(
        SelectedDownloadArtifactKind::AssetIndex,
        &destination,
        &url,
        "fixture-assets",
        &expected,
        Some(&fact_tx),
    )
    .await
    .expect("prepare destination capability");
    let source = acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
        client: &build_http_client(Duration::from_secs(5)),
        kind: SelectedDownloadArtifactKind::AssetIndex,
        url: &url,
        logical_identity: "fixture-assets",
        expected: &expected,
        max_bytes: 1024,
        target: prepared.target(),
        fact_tx: Some(&fact_tx),
    })
    .await
    .expect("acquire source");

    materialize_authenticated_selected_artifact_source(prepared, source, Some(&fact_tx))
        .await
        .err()
        .expect("materialization must fail");

    assert!(
        std::iter::from_fn(|| fact_rx.try_recv().ok())
            .all(|fact| fact.kind != ExecutionDownloadFactKind::ArtifactVerified)
    );
    let _ = fs::remove_dir_all(root);
}

async fn selected_materialization_fixture(
    destination: &Path,
    body: &[u8],
) -> (
    PreparedSelectedArtifactInstall,
    AuthenticatedSelectedArtifactSource,
) {
    let body = body.to_vec();
    let expected = ExpectedIntegrity::from_mojang(body.len() as i64, &sha1_hex(&body));
    let (url, _) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;
    let prepared = prepare_selected_artifact_install(
        SelectedDownloadArtifactKind::AssetIndex,
        destination,
        &url,
        "fixture-assets",
        &expected,
        None,
    )
    .await
    .expect("prepare selected materialization");
    let source = acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
        client: &build_http_client(Duration::from_secs(5)),
        kind: SelectedDownloadArtifactKind::AssetIndex,
        url: &url,
        logical_identity: "fixture-assets",
        expected: &expected,
        max_bytes: 1024,
        target: prepared.target(),
        fact_tx: None,
    })
    .await
    .expect("acquire selected materialization source");
    (prepared, source)
}

fn selected_promotion_control(
    hook: impl FnMut(SelectedPromotionTestStage, &Path, &Path) + Send + 'static,
) -> SelectedPromotionTestControl {
    SelectedPromotionTestControl {
        hook: Some(Box::new(hook)),
        pause_at: None,
        reached: None,
        resume: None,
        fail_publish_rename: false,
    }
}

fn selected_reserved_backups(destination: &Path) -> Vec<PathBuf> {
    let prefix = format!(
        "{}.axial-backup-",
        destination
            .file_name()
            .expect("selected destination name")
            .to_string_lossy()
    );
    fs::read_dir(destination.parent().expect("selected destination parent"))
        .expect("selected destination directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect()
}

#[tokio::test]
async fn authenticated_source_rejects_temp_path_substitution_before_namespace_mutation() {
    let root = temp_dir("selected-temp-substitution");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let retained = root.join("retained-authenticated-temp");
    let retained_hook = retained.clone();
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let control = selected_promotion_control(move |stage, temp, _destination| {
        if stage == SelectedPromotionTestStage::TempWritten {
            fs::rename(temp, &retained_hook).expect("retain authenticated temp");
            fs::write(temp, b"new-source").expect("substitute same-byte temp");
        }
    });

    materialize_authenticated_selected_artifact_source_with_control(
        prepared,
        source,
        Some(&fact_tx),
        control,
    )
    .await
    .err()
    .expect("substituted temp must fail");

    assert_eq!(
        fs::read(&destination).expect("old destination"),
        b"old-source"
    );
    assert_eq!(
        fs::read(&retained).expect("retained proof bytes"),
        b"new-source"
    );
    let facts = std::iter::from_fn(|| fact_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::PromoteFailed)
    );
    assert!(
        facts
            .iter()
            .all(|fact| fact.kind != ExecutionDownloadFactKind::ChecksumMismatch)
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_post_publish_corruption_restores_exact_backup() {
    let root = temp_dir("selected-post-publish-restore");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let control = selected_promotion_control(|stage, _temp, destination| {
        if stage == SelectedPromotionTestStage::PublishedUnverified {
            fs::write(destination, b"bad-source").expect("corrupt published handle");
        }
    });

    materialize_authenticated_selected_artifact_source_with_control(
        prepared, source, None, control,
    )
    .await
    .err()
    .expect("post-publish corruption must fail");

    assert_eq!(
        fs::read(&destination).expect("restored destination"),
        b"old-source"
    );
    assert_eq!(
        fs::read(selected_promotion_temp_path(&destination)).expect("retained rejected identity"),
        b"bad-source"
    );
    assert!(selected_reserved_backups(&destination).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_post_publish_corruption_restores_absence() {
    let root = temp_dir("selected-post-publish-absence");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let control = selected_promotion_control(|stage, _temp, destination| {
        if stage == SelectedPromotionTestStage::PublishedUnverified {
            fs::write(destination, b"bad-source").expect("corrupt published handle");
        }
    });

    materialize_authenticated_selected_artifact_source_with_control(
        prepared, source, None, control,
    )
    .await
    .err()
    .expect("post-publish corruption must fail");

    assert!(!destination.exists());
    assert_eq!(
        fs::read(selected_promotion_temp_path(&destination)).expect("retained rejected identity"),
        b"bad-source"
    );
    assert!(selected_reserved_backups(&destination).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_forced_publish_failure_restores_backup_and_retains_temp() {
    let root = temp_dir("selected-forced-publish-restore");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let mut control = selected_promotion_control(|_, _, _| {});
    control.fail_publish_rename = true;

    materialize_authenticated_selected_artifact_source_with_control(
        prepared, source, None, control,
    )
    .await
    .err()
    .expect("forced publish failure must fail");

    assert_eq!(
        fs::read(&destination).expect("restored destination"),
        b"old-source"
    );
    assert_eq!(
        fs::read(selected_promotion_temp_path(&destination)).expect("retained authenticated temp"),
        b"new-source"
    );
    assert!(selected_reserved_backups(&destination).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_forced_publish_failure_restores_absence_and_retains_temp() {
    let root = temp_dir("selected-forced-publish-absence");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let mut control = selected_promotion_control(|_, _, _| {});
    control.fail_publish_rename = true;

    materialize_authenticated_selected_artifact_source_with_control(
        prepared, source, None, control,
    )
    .await
    .err()
    .expect("forced publish failure must fail");

    assert!(!destination.exists());
    assert_eq!(
        fs::read(selected_promotion_temp_path(&destination)).expect("retained authenticated temp"),
        b"new-source"
    );
    assert!(selected_reserved_backups(&destination).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_destination_substitution_never_deletes_foreign_replacement() {
    let root = temp_dir("selected-destination-substitution");
    let destination = root.join("asset-index.json");
    let displaced = root.join("displaced-authenticated-source");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let displaced_hook = displaced.clone();
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let control = selected_promotion_control(move |stage, _temp, destination| {
        if stage == SelectedPromotionTestStage::PublishedUnverified {
            fs::rename(destination, &displaced_hook).expect("displace authenticated publication");
            fs::write(destination, b"foreign!!").expect("foreign replacement");
        }
    });

    materialize_authenticated_selected_artifact_source_with_control(
        prepared,
        source,
        Some(&fact_tx),
        control,
    )
    .await
    .err()
    .expect("destination substitution must fail closed");

    assert_eq!(
        fs::read(&destination).expect("foreign replacement"),
        b"foreign!!"
    );
    assert_eq!(
        fs::read(&displaced).expect("displaced source"),
        b"new-source"
    );
    let backups = selected_reserved_backups(&destination);
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read(&backups[0]).expect("reserved exact backup"),
        b"old-source"
    );
    assert!(std::iter::from_fn(|| fact_rx.try_recv().ok()).all(|fact| {
        !matches!(
            fact.kind,
            ExecutionDownloadFactKind::Promoted | ExecutionDownloadFactKind::ArtifactVerified
        )
    }));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_cancellation_after_backup_still_settles_publication() {
    let root = temp_dir("selected-cancel-after-backup");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let (reached_tx, reached_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let control = SelectedPromotionTestControl {
        hook: None,
        pause_at: Some(SelectedPromotionTestStage::BackupOwned),
        reached: Some(reached_tx),
        resume: Some(resume_rx),
        fail_publish_rename: false,
    };
    let materialization = tokio::spawn(async move {
        materialize_authenticated_selected_artifact_source_with_control(
            prepared, source, None, control,
        )
        .await
    });
    reached_rx.await.expect("backup-owned boundary");
    materialization.abort();
    let _ = resume_tx.send(());

    timeout(Duration::from_secs(5), async {
        loop {
            if fs::read(&destination).ok().as_deref() == Some(b"new-source")
                && !selected_promotion_temp_path(&destination).exists()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned publication must settle after caller cancellation");
    let backups = selected_reserved_backups(&destination);
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read(&backups[0]).expect("retained backup"),
        b"old-source"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn retained_file_publisher_owns_publication_after_caller_cancellation() {
    use std::io::{Seek as _, SeekFrom, Write as _};

    let root = temp_dir("retained-publisher-cancel-after-backup");
    let destination = root.join("library.jar");
    let body = b"authenticated-library-source";
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-library").expect("old destination");
    let mut source = tempfile::tempfile().expect("anonymous retained source");
    source.write_all(body).expect("write retained source");
    source.flush().expect("flush retained source");
    source.sync_data().expect("sync retained source");
    source.seek(SeekFrom::Start(0)).expect("rewind source");
    let observed_sha1: [u8; 20] = Sha1::digest(body).into();
    let (reached_tx, reached_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let control = SelectedPromotionTestControl {
        hook: None,
        pause_at: Some(SelectedPromotionTestStage::BackupOwned),
        reached: Some(reached_tx),
        resume: Some(resume_rx),
        fail_publish_rename: false,
    };
    let destination_for_task = destination.clone();
    let publisher = tokio::spawn(async move {
        publish_authenticated_retained_file_for_test(
            source,
            destination_for_task,
            body.len() as u64,
            observed_sha1,
            "minecraft_library_test".to_string(),
            Some(control),
        )
        .await
    });
    reached_rx.await.expect("backup-owned boundary");
    publisher.abort();
    let _ = resume_tx.send(());

    timeout(Duration::from_secs(5), async {
        loop {
            if fs::read(&destination).ok().as_deref() == Some(body.as_slice())
                && !selected_promotion_temp_path(&destination).exists()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned publisher must settle after caller cancellation");
    assert_eq!(selected_reserved_backups(&destination).len(), 1);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn prepared_library_publication_reuses_exact_destination_with_occupied_backup() {
    let root = temp_dir("prepared-library-publication-noop");
    let relative_path =
        ArtifactRelativePath::from_path(Path::new("org/example/demo/1.0.0/demo-1.0.0.jar"))
            .expect("safe relative path");
    let destination = relative_path.join_under(&libraries_dir(&root));
    let body = checksumless_test_jar();
    let expected = ExpectedIntegrity {
        size: Some(body.len() as u64),
        sha1: None,
    };
    let (url, _) = spawn_scripted_download_server(vec![
        ScriptedDownloadResponse::full("200 OK", body.clone()),
        ScriptedDownloadResponse::full("200 OK", body.clone()),
    ])
    .await;
    let client = build_http_client(Duration::from_secs(5));
    let pool = LibrarySourcePool::new();
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();

    let prepared = prepare_library_publication(
        &root,
        relative_path.clone(),
        &url,
        &expected,
        false,
        Some(&fact_tx),
    )
    .await
    .expect("prepare first publication");
    let source = acquire_authenticated_library_source(LibrarySourceRequest {
        client: &client,
        url: &url,
        expected: prepared.expected(),
        relative_path: &relative_path,
        max_bytes: 1024 * 1024,
        target: prepared.target(),
        pool: &pool,
        fact_tx: None,
    })
    .await
    .expect("first authenticated source");
    let (proof, outcome) =
        materialize_authenticated_library_source(prepared, source, Some(&fact_tx))
            .await
            .expect("publish first source");
    assert_eq!(outcome, ExactPublicationOutcome::Promoted);
    assert_eq!(proof.into_parts().0, relative_path);
    assert_eq!(fs::read(&destination).expect("published library"), body);

    let occupied_backup = promotion_backup_path(&destination);
    fs::write(&occupied_backup, b"occupied-backup").expect("occupied retained backup");
    let prepared = prepare_library_publication(
        &root,
        relative_path.clone(),
        &url,
        &expected,
        false,
        Some(&fact_tx),
    )
    .await
    .expect("prepare repeated publication");
    let source = acquire_authenticated_library_source(LibrarySourceRequest {
        client: &client,
        url: &url,
        expected: prepared.expected(),
        relative_path: &relative_path,
        max_bytes: 1024 * 1024,
        target: prepared.target(),
        pool: &pool,
        fact_tx: None,
    })
    .await
    .expect("repeated authenticated source");
    let (_, outcome) = materialize_authenticated_library_source(prepared, source, Some(&fact_tx))
        .await
        .expect("reuse exact destination");
    assert_eq!(outcome, ExactPublicationOutcome::AlreadyExact);
    assert_eq!(
        fs::read(&occupied_backup).expect("untouched backup"),
        b"occupied-backup"
    );
    let facts = std::iter::from_fn(|| fact_rx.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
            .count(),
        1
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn prepared_library_publication_rejects_mismatched_source_contract() {
    let root = temp_dir("prepared-library-publication-mismatch");
    let relative_path =
        ArtifactRelativePath::from_path(Path::new("org/example/demo/1.0.0/demo-1.0.0.jar"))
            .expect("safe relative path");
    let destination = relative_path.join_under(&libraries_dir(&root));
    let body = checksumless_test_jar();
    let prepared_expected = ExpectedIntegrity::default();
    let source_expected = ExpectedIntegrity {
        size: Some(body.len() as u64),
        sha1: None,
    };
    let (url, _) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;
    let client = build_http_client(Duration::from_secs(5));
    let pool = LibrarySourcePool::new();
    let prepared = prepare_library_publication(
        &root,
        relative_path.clone(),
        &url,
        &prepared_expected,
        false,
        None,
    )
    .await
    .expect("prepare destination contract");
    let source = acquire_authenticated_library_source(LibrarySourceRequest {
        client: &client,
        url: &url,
        expected: &source_expected,
        relative_path: &relative_path,
        max_bytes: 1024 * 1024,
        target: prepared.target(),
        pool: &pool,
        fact_tx: None,
    })
    .await
    .expect("authenticated mismatched source");

    materialize_authenticated_library_source(prepared, source, None)
        .await
        .err()
        .expect("mismatched expected contract must fail");
    assert!(!destination.exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn prepared_library_publication_rejects_colliding_relative_path() {
    let root = temp_dir("prepared-library-relative-path-mismatch");
    let prepared_path =
        ArtifactRelativePath::from_path(Path::new("org/example/first/1.0.0/shared.jar"))
            .expect("safe prepared path");
    let source_path =
        ArtifactRelativePath::from_path(Path::new("org/example/second/1.0.0/shared.jar"))
            .expect("safe source path");
    let destination = prepared_path.join_under(&libraries_dir(&root));
    let body = checksumless_test_jar();
    let expected = ExpectedIntegrity {
        size: Some(body.len() as u64),
        sha1: None,
    };
    let (url, _) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;
    let client = build_http_client(Duration::from_secs(5));
    let pool = LibrarySourcePool::new();
    let prepared = prepare_library_publication(&root, prepared_path, &url, &expected, false, None)
        .await
        .expect("prepare destination contract");
    let source = acquire_authenticated_library_source(LibrarySourceRequest {
        client: &client,
        url: &url,
        expected: &expected,
        relative_path: &source_path,
        max_bytes: 1024 * 1024,
        target: prepared.target(),
        pool: &pool,
        fact_tx: None,
    })
    .await
    .expect("authenticated colliding source");

    materialize_authenticated_library_source(prepared, source, None)
        .await
        .err()
        .expect("colliding target label must not replace the logical path key");
    assert!(!destination.exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn prepared_library_publication_rejects_provider_url_substitution() {
    let root = temp_dir("prepared-library-provider-mismatch");
    let relative_path =
        ArtifactRelativePath::from_path(Path::new("org/example/provider/1.0.0/provider.jar"))
            .expect("safe relative path");
    let destination = relative_path.join_under(&libraries_dir(&root));
    let body = checksumless_test_jar();
    let expected = ExpectedIntegrity::default();
    let (source_url, _) =
        spawn_scripted_download_server(vec![ScriptedDownloadResponse::full("200 OK", body)]).await;
    let prepared = prepare_library_publication(
        &root,
        relative_path.clone(),
        "https://provider-b.invalid/library.jar",
        &expected,
        false,
        None,
    )
    .await
    .expect("prepare provider-bound contract");
    let client = build_http_client(Duration::from_secs(5));
    let pool = LibrarySourcePool::new();
    let source = acquire_authenticated_library_source(LibrarySourceRequest {
        client: &client,
        url: &source_url,
        expected: &expected,
        relative_path: &relative_path,
        max_bytes: 1024 * 1024,
        target: prepared.target(),
        pool: &pool,
        fact_tx: None,
    })
    .await
    .expect("authenticated provider-A source");

    materialize_authenticated_library_source(prepared, source, None)
        .await
        .err()
        .expect("provider substitution must fail");
    assert!(!destination.exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_missing_temp_after_backup_restores_destination() {
    let root = temp_dir("selected-missing-temp-after-backup");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let control = selected_promotion_control(|stage, temp, _destination| {
        if stage == SelectedPromotionTestStage::BackupOwned {
            fs::remove_file(temp).expect("remove authenticated temp at backup boundary");
        }
    });

    materialize_authenticated_selected_artifact_source_with_control(
        prepared, source, None, control,
    )
    .await
    .err()
    .expect("missing temp must fail");

    assert_eq!(
        fs::read(&destination).expect("restored destination"),
        b"old-source"
    );
    assert!(selected_reserved_backups(&destination).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_missing_publication_restores_destination() {
    let root = temp_dir("selected-missing-publication");
    let destination = root.join("asset-index.json");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let control = selected_promotion_control(|stage, _temp, destination| {
        if stage == SelectedPromotionTestStage::PublishedUnverified {
            fs::remove_file(destination).expect("remove unverified publication");
        }
    });

    materialize_authenticated_selected_artifact_source_with_control(
        prepared, source, None, control,
    )
    .await
    .err()
    .expect("missing publication must fail");

    assert_eq!(
        fs::read(&destination).expect("restored destination"),
        b"old-source"
    );
    assert!(selected_reserved_backups(&destination).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_source_foreign_backup_substitution_is_nonterminal_and_retained() {
    let root = temp_dir("selected-foreign-backup-cleanup");
    let destination = root.join("asset-index.json");
    let displaced_backup = root.join("displaced-exact-backup");
    fs::create_dir_all(&root).expect("source root");
    fs::write(&destination, b"old-source").expect("old destination");
    let (prepared, source) = selected_materialization_fixture(&destination, b"new-source").await;
    let displaced_hook = displaced_backup.clone();
    let control = selected_promotion_control(move |stage, _temp, destination| {
        if stage == SelectedPromotionTestStage::PublishedVerified {
            let backup = selected_reserved_backups(destination)
                .into_iter()
                .next()
                .expect("exact backup");
            fs::rename(&backup, &displaced_hook).expect("displace exact backup");
            fs::write(&backup, b"foreign-backup").expect("foreign backup replacement");
        }
    });

    materialize_authenticated_selected_artifact_source_with_control(
        prepared, source, None, control,
    )
    .await
    .expect("verified publication is already committed");

    assert_eq!(
        fs::read(&destination).expect("published destination"),
        b"new-source"
    );
    assert_eq!(
        fs::read(&displaced_backup).expect("displaced exact backup"),
        b"old-source"
    );
    let backups = selected_reserved_backups(&destination);
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read(&backups[0]).expect("foreign backup"),
        b"foreign-backup"
    );
    let _ = fs::remove_dir_all(root);
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
