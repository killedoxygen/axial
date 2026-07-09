use super::assets::{
    AssetObjectDownloadJob, copy_virtual_asset_if_missing, copy_virtual_assets,
    missing_asset_object_jobs, repair_virtual_assets_from_index, unique_asset_object_jobs,
    virtual_asset_destination,
};
use super::client::{adaptive_download_concurrency, build_http_client};
use super::facts::{ExecutionDownloadRequest, execution_download_fact};
use super::install::version_json_download_from_manifest_entry;
use super::integrity::{
    download_size_mismatch, existing_file_satisfies, hash_file, verify_download_integrity,
};
use super::libraries::{library_jobs_for, resolve_library_download, resolve_native_download};
use super::model::{ActualIntegrity, DownloadIntegrityError};
use super::path_safety::{bounded_download_file_label, safe_download_target_label};
use super::runtime::{RuntimeEnsurePipeline, finish_runtime_pipeline_after_artifacts};
use super::transfer::{
    download_file_with_client, download_file_with_client_and_fact_sender, execute_download_to_temp,
    remove_stale_download_temp,
};
use super::*;
use crate::launch::{JavaVersion, Library, LibraryArtifact, LibraryDownload, maven_to_path};
use crate::manifest::ManifestEntry;
use crate::paths::versions_dir;
use crate::rules::Environment;
use sha1::{Digest as _, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn install_version_emits_terminal_error_when_setup_fails() {
    let root = temp_dir("setup-failure");
    fs::create_dir_all(&root).expect("create root");
    fs::write(versions_dir(&root), b"not a directory").expect("write versions sentinel");

    let downloader = Downloader::new(&root);
    let mut events = Vec::new();
    let result = downloader
        .install_version("1.20.1", None, |progress| events.push(progress))
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
async fn install_version_starts_asset_index_before_library_download_finishes() {
    let root = temp_dir("overlap-assets-libraries");
    let (version_url, mut requests, release_library) = spawn_overlapped_install_server().await;
    let downloader = Downloader::new(&root);
    let install = tokio::spawn(async move {
        downloader
            .install_version("overlap", Some(&version_url), |_| {})
            .await
    });

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
async fn install_version_with_facts_emits_private_download_facts_only() {
    let root = temp_dir("install-private-facts");
    let (version_url, _requests, release_library) = spawn_overlapped_install_server().await;
    release_library
        .send(())
        .expect("release library response before request");
    let downloader = Downloader::new(&root);
    let mut events = Vec::new();
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();

    downloader
        .install_version_with_facts_and_descriptors(
            "overlap",
            Some(&version_url),
            |progress| events.push(progress),
            |fact| facts.push(fact),
            |descriptor| descriptors.push(descriptor),
        )
        .await
        .expect("install should succeed");

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
            .any(|fact| fact.kind == ExecutionDownloadFactKind::MetadataMissing)
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
    assert!(descriptors.iter().any(|descriptor| {
        descriptor.kind == SelectedDownloadArtifactKind::AssetIndex && descriptor.sha1().len() == 40
    }));
    assert!(descriptors.iter().any(|descriptor| {
        descriptor.kind == SelectedDownloadArtifactKind::Library
            && descriptor.destination().ends_with("lib-1.0.0.jar")
    }));
    let debug = format!("{:?}", descriptors[0]).to_ascii_lowercase();
    assert!(!debug.contains(root.to_string_lossy().as_ref()));
    assert!(!debug.contains("http://"));
    assert!(!debug.contains(descriptors[0].sha1()));
    let progress_json = serde_json::to_string(&events).expect("progress json");
    assert!(!progress_json.contains("facts"));
    assert!(!progress_json.contains("descriptors"));
    assert!(!progress_json.contains("sha1"));

    let _ = fs::remove_dir_all(root);
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
        3,
    )
    .await;
    let client = build_http_client(Duration::from_secs(5));
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();

    let result = download_file_with_client_and_fact_sender(
        SelectedDownloadArtifactKind::ClientJar,
        &client,
        &url,
        &destination,
        &expected,
        Some(&fact_tx),
        Some(&descriptor_tx),
    )
    .await;

    assert!(result.is_err());
    drop(fact_tx);
    drop(descriptor_tx);
    let mut facts = Vec::new();
    while let Some(fact) = fact_rx.recv().await {
        facts.push(fact);
    }
    let mut descriptors = Vec::new();
    while let Some(descriptor) = descriptor_rx.recv().await {
        descriptors.push(descriptor);
    }
    assert!(facts.iter().any(|fact| {
        fact.kind == ExecutionDownloadFactKind::ArtifactMissing
            && fact.target == "minecraft_client_artifact"
    }));
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::ProviderFailure)
    );
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].kind, SelectedDownloadArtifactKind::ClientJar);
    assert_eq!(descriptors[0].target, "minecraft_client_artifact");
    assert!(!destination.exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn selected_existing_corrupt_artifact_emits_fact_without_replacement() {
    let root = temp_dir("selected-corrupt-artifact-fact");
    fs::create_dir_all(&root).expect("create root");
    let destination = root.join("artifact.jar");
    fs::write(&destination, b"wrong").expect("write corrupt artifact");
    let expected = ExpectedIntegrity::from_mojang(5, &sha1_hex(b"fresh"));
    let client = build_http_client(Duration::from_secs(1));
    let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();

    let result = download_file_with_client_and_fact_sender(
        SelectedDownloadArtifactKind::ClientJar,
        &client,
        "http://127.0.0.1:9/artifact.jar",
        &destination,
        &expected,
        Some(&fact_tx),
        Some(&descriptor_tx),
    )
    .await;

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
    assert_eq!(
        fs::read(&destination).expect("corrupt artifact preserved"),
        b"wrong"
    );
    drop(fact_tx);
    drop(descriptor_tx);
    let mut facts = Vec::new();
    while let Some(fact) = fact_rx.recv().await {
        facts.push(fact);
    }
    let mut descriptors = Vec::new();
    while let Some(descriptor) = descriptor_rx.recv().await {
        descriptors.push(descriptor);
    }
    assert!(facts.iter().any(|fact| {
        fact.kind == ExecutionDownloadFactKind::ChecksumMismatch
            && fact.target == "minecraft_client_artifact"
            && fact
                .fields
                .iter()
                .any(|(key, value)| key == "algorithm" && value == "sha1")
    }));
    assert!(
        !facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::NetworkFailure)
    );
    assert!(
        !facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
    );
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].kind, SelectedDownloadArtifactKind::ClientJar);
    assert_eq!(descriptors[0].target, "minecraft_client_artifact");

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
    let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();

    let result = download_file_with_client_and_fact_sender(
        SelectedDownloadArtifactKind::ClientJar,
        &client,
        "http://127.0.0.1:9/artifact.jar",
        &destination,
        &expected,
        Some(&fact_tx),
        Some(&descriptor_tx),
    )
    .await;

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
    assert!(destination.is_dir());
    drop(fact_tx);
    drop(descriptor_tx);
    let mut facts = Vec::new();
    while let Some(fact) = fact_rx.recv().await {
        facts.push(fact);
    }
    let mut descriptors = Vec::new();
    while let Some(descriptor) = descriptor_rx.recv().await {
        descriptors.push(descriptor);
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
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].target, "minecraft_client_artifact");

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
        std::future::pending::<Result<JavaVersion, String>>().await
    });
    started_rx.await.expect("runtime task should start");
    let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());

    let result = timeout(
        Duration::from_millis(100),
        finish_runtime_pipeline_after_artifacts(
            Some(runtime_pipeline(task)),
            Err(artifact_error),
            &mut |_| {},
        ),
    )
    .await
    .expect("artifact error should return without waiting for runtime task");

    assert!(matches!(
        result,
        Err(DownloadError::ResolveManifest(message)) if message == "artifact failed"
    ));
    timeout(Duration::from_millis(100), async {
        while !cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime task should be aborted");
}

#[tokio::test]
async fn runtime_error_is_reported_when_artifact_install_succeeds() {
    let task = tokio::spawn(async { Err::<JavaVersion, String>("runtime failed".to_string()) });

    let result =
        finish_runtime_pipeline_after_artifacts(Some(runtime_pipeline(task)), Ok(()), &mut |_| {})
            .await;

    assert!(matches!(
        result,
        Err(DownloadError::PrepareRuntime(message)) if message == "runtime failed"
    ));
}

#[tokio::test]
async fn artifact_error_is_preserved_when_runtime_also_fails() {
    let task = tokio::spawn(async { Err::<JavaVersion, String>("runtime failed".to_string()) });
    let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());

    let result = finish_runtime_pipeline_after_artifacts(
        Some(runtime_pipeline(task)),
        Err(artifact_error),
        &mut |_| {},
    )
    .await;

    assert!(matches!(
        result,
        Err(DownloadError::ResolveManifest(message)) if message == "artifact failed"
    ));
}

fn runtime_pipeline(
    task: tokio::task::JoinHandle<Result<JavaVersion, String>>,
) -> RuntimeEnsurePipeline {
    let (_progress_tx, progress_rx) = mpsc::unbounded_channel();
    RuntimeEnsurePipeline { task, progress_rx }
}

#[test]
fn mixed_windows_native_libraries_only_download_matching_arch() {
    let env = Environment {
        os_name: "windows".to_string(),
        os_arch: "x86_64".to_string(),
        os_version: String::new(),
        features: HashMap::new(),
    };
    let mc_dir = Path::new("/tmp/croopor-test");
    let libraries = vec![
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-arm64"),
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-x86"),
        native_library("org.lwjgl:lwjgl:3.3.3:natives-windows"),
    ];

    let jobs = library_jobs_for(mc_dir, &libraries, &env);
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

    let job = resolve_native_download(&lib, Path::new("/tmp/croopor-test"), "windows")
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
    let mc_dir = Path::new("/tmp/croopor-test");
    let libraries = vec![
        normal_library("org.example:duplicate:1.0.0"),
        normal_library("org.example:duplicate:1.0.0"),
    ];

    let jobs = library_jobs_for(mc_dir, &libraries, &env);

    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].name.contains("duplicate-1.0.0.jar"));
}

#[test]
fn unique_asset_object_jobs_deduplicate_same_hash() {
    let objects_dir = Path::new("/tmp/croopor-test/assets/objects");
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
fn unique_asset_object_jobs_rejects_one_character_hash() {
    let objects_dir = Path::new("/tmp/croopor-test/assets/objects");
    let result = unique_asset_object_jobs(objects_dir, [("a", 4)]);

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[test]
fn unique_asset_object_jobs_rejects_non_hex_hash() {
    let objects_dir = Path::new("/tmp/croopor-test/assets/objects");
    let result = unique_asset_object_jobs(
        objects_dir,
        [("abcdef1234567890abcdef1234567890abcdef1z", 4)],
    );

    assert!(matches!(result, Err(DownloadError::Integrity(_))));
}

#[tokio::test]
async fn missing_asset_object_jobs_uses_bounded_size_prefilter() {
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

    assert_eq!(paths.len(), 3);
    assert!(paths.contains(&missing_path));
    assert!(paths.contains(&wrong_size_path));
    assert!(paths.contains(&wrong_hash_same_size_path));

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
    let tmp_path = destination.with_extension("tmp");
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
    let tmp_path = destination.with_extension("tmp");
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
    let tmp_path = destination.with_extension("tmp");
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
    assert!(!destination.with_extension("tmp").exists());

    let _ = fs::remove_dir_all(root);
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
    assert!(!destination.with_extension("tmp").exists());

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
    assert!(!destination.with_extension("tmp").exists());

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
    assert!(!destination.with_extension("tmp").exists());

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
                require_checksum: true,
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
        assert!(!destination.with_extension("tmp").exists());
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
fn download_integrity_futures_stay_small_enough_for_tokio_workers() {
    let path = Path::new("/tmp/croopor-test/artifact.jar");
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
    let downloader = Downloader::new(&root);
    assert!(
        std::mem::size_of_val(&downloader.install_version("1.21.1", None, |_| {})) < 8192,
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
    let root = Path::new("/tmp/croopor-test/assets/virtual/legacy");

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
    assert!(!destination.with_extension("tmp").exists());

    let _ = fs::remove_dir_all(root);
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
    assert!(!destination.with_extension("tmp").exists());
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
    let path = Path::new("/tmp/croopor-test/artifact.jar");
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

    let job = resolve_library_download(&lib, Path::new("/tmp/croopor-test")).expect("library job");

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
        checksums: vec!["not-a-sha1".to_string(), sha1.to_string()],
        ..Library::default()
    };

    let job = resolve_library_download(&lib, Path::new("/tmp/croopor-test")).expect("library job");

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

    let job = resolve_native_download(&lib, Path::new("/tmp/croopor-test"), "windows")
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

    let job = resolve_library_download(&lib, Path::new("/tmp/croopor-test")).expect("library job");

    assert_eq!(job.expected, ExpectedIntegrity::default());
    assert!(!job.expected.has_evidence());
}

#[test]
fn expected_integrity_ignores_default_mojang_metadata() {
    let expected = ExpectedIntegrity::from_mojang(0, " ");

    assert_eq!(expected, ExpectedIntegrity::default());
    assert!(!expected.has_evidence());
}

#[test]
fn manifest_entry_download_carries_sha1_without_forcing_download() {
    let sha1 = "abcdef1234567890abcdef1234567890abcdef12";
    let download = version_json_download_from_manifest_entry(ManifestEntry {
        id: "1.20.1".to_string(),
        kind: "release".to_string(),
        url: "https://example.invalid/1.20.1.json".to_string(),
        time: String::new(),
        release_time: String::new(),
        sha1: sha1.to_string(),
        compliance_level: 1,
    });

    assert_eq!(download.url, "https://example.invalid/1.20.1.json");
    assert_eq!(download.expected, ExpectedIntegrity::from_sha1(sha1));
    assert!(!download.force_download);
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
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
        "croopor-download-{prefix}-{}-{nanos:x}",
        std::process::id()
    ))
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

async fn spawn_overlapped_install_server()
-> (String, mpsc::UnboundedReceiver<String>, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind install overlap server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let (release_library_tx, release_library_rx) = oneshot::channel();
    let library_body = b"library".to_vec();
    let library_sha1 = sha1_hex(&library_body);
    let asset_index_body = br#"{"objects":{}}"#.to_vec();
    let asset_index_sha1 = sha1_hex(&asset_index_body);
    let version_body = serde_json::json!({
        "id": "overlap",
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

    tokio::spawn(async move {
        let mut release_library_rx = Some(release_library_rx);
        for _ in 0..4 {
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
                "/libraries/lib.jar" => {
                    let receiver = release_library_rx.take();
                    let body = library_body.clone();
                    tokio::spawn(async move {
                        if let Some(receiver) = receiver {
                            let _ = receiver.await;
                        }
                        write_raw_response(&mut socket, "200 OK", &body).await;
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
        request_rx,
        release_library_tx,
    )
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
