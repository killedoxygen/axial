use super::*;
use crate::network::ClientConfig;
use axial_minecraft::managed_path::ManagedLibraryTestAuthority;
use axial_minecraft::{
    LoaderComponentId, VersionBundlePublicationGuardForTest, installed_version_id_for,
};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

pub(crate) fn manifest(entries: &[(&str, &str)]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "latest": { "release": "", "snapshot": "" },
        "versions": entries.iter().map(|(id, kind)| json!({
            "id": id,
            "type": kind,
            "url": "https://piston-meta.mojang.com/v1/packages/0123456789012345678901234567890123456789/version.json",
            "sha1": "0123456789012345678901234567890123456789",
            "releaseTime": "2026-01-01T00:00:00+00:00"
        })).collect::<Vec<_>>()
    })).unwrap()
}

fn client() -> ProviderClient {
    ProviderClient::new(ClientConfig {
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(2),
        total_timeout: Duration::from_secs(3),
        ..ClientConfig::default()
    })
    .unwrap()
}

pub(crate) fn fixture_catalog(body: Vec<u8>) -> (Catalog, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "catalog did not request the fixture"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        };
        // BSD accept can inherit the listener's nonblocking flag. This fixture
        // uses a bounded blocking HTTP read after accepting its one connection.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0_u8; 4096];
        assert!(stream.read(&mut request).unwrap() > 0);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    let mut catalog = Catalog::new(client());
    catalog.source_fixture = Some((
        format!("{origin}/manifest.json"),
        OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
    ));
    (catalog, thread)
}

fn version_file(root: &Path, id: &str, value: Value, jar: bool) {
    let directory = root.join("versions").join(id);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join(format!("{id}.json")),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    if jar {
        fs::write(directory.join(format!("{id}.jar")), b"test jar presence").unwrap();
    }
}

fn make_cache_stale(root: &Path) {
    fs::File::open(root.join("cache/version_manifest_v2.json"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();
}

#[test]
fn catalog_futures_keep_provider_state_off_the_callers_stack() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    let catalog = Catalog::new(client());
    let cancel = CancellationToken::new();
    let descriptor = VersionDescriptor::new(
        decode_manifest(&manifest(&[("1.21.11", "release")]))
            .unwrap()
            .versions
            .remove(0),
    );
    let mut loaders = Vec::new();
    let sizes = [
        (
            "snapshot",
            std::mem::size_of_val(&catalog.snapshot(authority.operation(), &cancel)),
        ),
        (
            "resolve_install",
            std::mem::size_of_val(&catalog.resolve_install(
                authority.operation(),
                "1.21.11",
                &cancel,
            )),
        ),
        (
            "fetch_version",
            std::mem::size_of_val(&catalog.fetch_version(&descriptor, &cancel)),
        ),
        (
            "enrich_loader_versions",
            std::mem::size_of_val(&catalog.enrich_loader_versions(
                authority.operation(),
                &mut loaders,
                &cancel,
            )),
        ),
    ];
    for (boundary, size) in sizes {
        assert!(
            size <= 64 * 1024,
            "catalog {boundary} future retains {size} bytes before API/setup nesting"
        );
    }
}

#[test]
fn all_retained_families_and_variants_have_backend_authored_labels() {
    let cases = [
        ("1.21.11", "release", "release", "stable"),
        ("1.7.10_pre4", "snapshot", "pre_release", "preview"),
        ("1.21.11-rc3", "snapshot", "release_candidate", "preview"),
        ("26.1-snapshot-9", "snapshot", "release_snapshot", "preview"),
        ("25w46a", "snapshot", "weekly_snapshot", "preview"),
        ("24w14potato", "snapshot", "potato_snapshot", "preview"),
        ("1.16_combat-3", "snapshot", "combat_test", "experimental"),
        (
            "1.18_experimentaI-snapshot-6",
            "snapshot",
            "experimental_snapshot",
            "experimental",
        ),
        (
            "1.19_deep_dark_experimental_snapshot-1",
            "snapshot",
            "deep_dark_experimental_snapshot",
            "experimental",
        ),
        ("b1.7.3", "old_beta", "old_beta", "legacy"),
        ("a1.2.6", "old_alpha", "old_alpha", "legacy"),
        ("classic-server-test", "old_beta", "old_beta", "legacy"),
        ("unrecognized-snapshot", "snapshot", "snapshot", "preview"),
        ("custom-id", "", "release", "unknown"),
    ];
    let bytes = manifest(&cases.iter().map(|row| (row.0, row.1)).collect::<Vec<_>>());
    let decoded = decode_manifest(&bytes).unwrap();
    let rows = model::catalog_rows(&decoded);
    for (row, expected) in rows.iter().zip(cases) {
        assert_eq!(row.id, expected.0, "catalog retains provider order");
        assert_eq!(row.minecraft_meta.family, expected.2);
        assert_eq!(
            serde_json::to_value(row.lifecycle.channel).unwrap(),
            expected.3
        );
        assert!(!row.minecraft_meta.display_name.is_empty());
    }
    let variants = decode_manifest(&manifest(&[
        ("1.21.11_original", "release"),
        ("1.21.11_unobfuscated", "release"),
    ]))
    .unwrap();
    let rows = model::catalog_rows(&variants);
    assert_eq!(rows[0].minecraft_meta.variant_kind, "original");
    assert_eq!(rows[1].minecraft_meta.variant_kind, "unobfuscated");
    assert_eq!(rows[1].minecraft_meta.display_name, "1.21.11");
    assert_eq!(rows[1].minecraft_meta.display_hint, "Unobfuscated");
}

#[test]
fn invalid_provider_identity_and_metadata_sources_never_become_descriptors() {
    for id in ["../outside", "CON", "1.21.", "1.21/child"] {
        assert!(
            decode_manifest(&manifest(&[(id, "release")])).is_err(),
            "{id}"
        );
    }
    assert!(decode_manifest(&manifest(&[("1.21", "release"), ("1.21", "release")])).is_err());
    for url in [
        "http://piston-meta.mojang.com/a",
        "https://evil.test/a",
        "https://piston-meta.mojang.com.evil.test/a",
        "https://user:secret@piston-meta.mojang.com/a",
    ] {
        let mut value: Value = serde_json::from_slice(&manifest(&[("1.21", "release")])).unwrap();
        value["versions"][0]["url"] = url.into();
        assert!(decode_manifest(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut value: Value = serde_json::from_slice(&manifest(&[("1.21", "release")])).unwrap();
    value["versions"][0]["sha1"] = "bad".into();
    assert!(decode_manifest(&serde_json::to_vec(&value).unwrap()).is_err());
    value["versions"][0]["sha1"] = "0123456789012345678901234567890123456789".into();
    value["latest"]["release"] = "missing".into();
    assert!(decode_manifest(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[tokio::test]
async fn live_manifest_is_persisted_and_reused_by_a_new_catalog_instance() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    let (catalog, server) = fixture_catalog(manifest(&[("1.21.11", "release")]));
    let snapshot = catalog
        .snapshot(authority.operation(), &CancellationToken::new())
        .await;
    server.join().unwrap();
    assert_eq!(snapshot.catalog_state.state_id, CatalogStateId::Ready);
    assert!(!snapshot.catalog_state.cache_hit);
    let restarted = Catalog::new(client());
    let cached = restarted
        .snapshot(authority.operation(), &CancellationToken::new())
        .await;
    assert!(cached.catalog_state.fresh && cached.catalog_state.cache_hit);
    assert_eq!(cached.versions, snapshot.versions);
    let selected = restarted
        .resolve_install(authority.operation(), "1.21.11", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(selected.id(), "1.21.11");
    assert!(matches!(
        restarted
            .resolve_install(authority.operation(), "absent", &CancellationToken::new())
            .await,
        Err(CatalogError::UnknownVersion)
    ));
}

#[tokio::test]
async fn malformed_refresh_preserves_stale_records_without_poisoning_the_cache() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    let bytes = manifest(&[("1.21.11", "release")]);
    axial_minecraft::manifest::cache_version_manifest(authority.operation(), &bytes)
        .await
        .unwrap();
    make_cache_stale(root.path());
    let (catalog, server) = fixture_catalog(b"{ malformed".to_vec());
    let snapshot = catalog
        .snapshot(authority.operation(), &CancellationToken::new())
        .await;
    server.join().unwrap();
    assert_eq!(snapshot.catalog_state.state_id, CatalogStateId::Stale);
    assert_eq!(
        snapshot.catalog_state.failure,
        Some(CatalogFailure::Malformed)
    );
    assert!(snapshot.catalog_state.stale && !snapshot.catalog_state.fresh);
    assert_eq!(snapshot.versions[0].id, "1.21.11");
    assert_eq!(
        fs::read(root.path().join("cache/version_manifest_v2.json")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn valid_empty_catalog_is_distinct_from_malformed_response_and_missing_cache() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    let (catalog, server) = fixture_catalog(manifest(&[]));
    let empty = catalog
        .snapshot(authority.operation(), &CancellationToken::new())
        .await;
    server.join().unwrap();
    assert_eq!(empty.catalog_state.state_id, CatalogStateId::Empty);
    assert!(empty.catalog_state.empty && empty.catalog_state.fresh);
    make_cache_stale(root.path());
    let (catalog, server) = fixture_catalog(b"{}".to_vec());
    let stale_empty = catalog
        .snapshot(authority.operation(), &CancellationToken::new())
        .await;
    server.join().unwrap();
    assert_eq!(stale_empty.catalog_state.state_id, CatalogStateId::Stale);
    assert!(stale_empty.catalog_state.empty);

    let other_root =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let other = ManagedLibraryTestAuthority::open(other_root.path()).unwrap();
    let (catalog, server) = fixture_catalog(b"{}".to_vec());
    let malformed = catalog
        .snapshot(other.operation(), &CancellationToken::new())
        .await;
    server.join().unwrap();
    assert_eq!(malformed.catalog_state.state_id, CatalogStateId::Malformed);
    assert!(!malformed.catalog_state.empty);
    assert_eq!(
        catalog
            .cached_snapshot(other.operation())
            .await
            .catalog_state
            .state_id,
        CatalogStateId::Unavailable
    );
}

#[tokio::test]
async fn cancelled_request_does_not_publish_a_cache() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let snapshot = Catalog::new(client())
        .snapshot(authority.operation(), &cancel)
        .await;
    assert_eq!(
        snapshot.catalog_state.failure,
        Some(CatalogFailure::Cancelled)
    );
    assert!(!root.path().join("cache/version_manifest_v2.json").exists());
}

#[tokio::test]
async fn installed_scan_preserves_all_loader_labels_and_inherited_metadata() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    version_file(
        root.path(),
        "1.21.5",
        json!({
            "id":"1.21.5", "type":"release", "javaVersion":{"component":"java-runtime-delta", "majorVersion":21},
            "releaseTime":"2025-03-25T12:00:00+00:00"
        }),
        true,
    );
    let cases = [
        (LoaderComponentId::Fabric, "0.19.3", "Fabric"),
        (LoaderComponentId::Quilt, "0.29.2", "Quilt"),
        (LoaderComponentId::Forge, "55.0.1-beta", "Forge"),
        (LoaderComponentId::NeoForge, "21.5.75", "NeoForge"),
    ];
    for (component, build, _) in cases {
        let id = installed_version_id_for(component, "1.21.5", build).unwrap();
        version_file(
            root.path(),
            &id,
            json!({ "id":id, "inheritsFrom":"1.21.5", "axialMaterialized":true }),
            false,
        );
    }
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    let installed = installed_versions(authority.operation(), None)
        .await
        .unwrap();
    assert_eq!(installed.scan_state.state_id, "ready");
    assert_eq!(installed.versions.len(), 5);
    for (component, build, label) in cases {
        let row = installed
            .versions
            .iter()
            .find(|row| {
                row.loader
                    .as_ref()
                    .is_some_and(|loader| loader.component_id == component)
            })
            .unwrap();
        let loader = row.loader.as_ref().unwrap();
        assert_eq!(loader.component_name, label);
        assert_eq!(loader.loader_version, build);
        assert_eq!(row.minecraft_meta.display_name, "1.21.5");
        assert_eq!(row.inherits_from, "1.21.5");
        assert_eq!(row.java_major, 21);
        assert!(row.launchable);
    }
}

#[tokio::test]
async fn malformed_loader_parent_and_traversal_are_degraded_not_vanilla() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let id = installed_version_id_for(LoaderComponentId::NeoForge, "1.21.5", "21.5.75").unwrap();
    version_file(
        root.path(),
        &id,
        json!({ "id":id, "inheritsFrom":"1.21.4", "axialMaterialized":true }),
        false,
    );
    version_file(
        root.path(),
        "unsafe-child",
        json!({ "id":"unsafe-child", "inheritsFrom":"../outside" }),
        false,
    );
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    let report = installed_versions(authority.operation(), None)
        .await
        .unwrap();
    assert!(report.scan_state.degraded);
    assert!(report.versions.is_empty());
    let wire = serde_json::to_string(&report).unwrap();
    assert!(!wire.contains("../outside"));
    assert!(!wire.contains("loader-v2"));
}

#[tokio::test]
async fn publication_in_progress_refuses_scan_and_missing_versions_is_empty() {
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
    assert_eq!(
        installed_versions(authority.operation(), None)
            .await
            .unwrap()
            .scan_state
            .state_id,
        "empty"
    );
    // The retained test guard opens an existing publication lane, just like a
    // second process contending with a real publisher. Empty-library scanning
    // deliberately does not create that mutable lane for us.
    fs::create_dir(root.path().join(".axial-publication")).unwrap();
    fs::write(root.path().join(".axial-publication/publication.lock"), b"").unwrap();
    let writer = VersionBundlePublicationGuardForTest::acquire(authority.operation()).unwrap();
    assert!(matches!(
        installed_versions(authority.operation(), None).await,
        Err(CatalogError::InstalledUnavailable)
    ));
    drop(writer);
    assert_eq!(
        installed_versions(authority.operation(), None)
            .await
            .unwrap()
            .scan_state
            .state_id,
        "empty"
    );
}

#[test]
fn metadata_json_must_match_the_selected_identity() {
    let descriptor = VersionDescriptor::new(
        decode_manifest(&manifest(&[("1.21.5", "release")]))
            .unwrap()
            .versions
            .remove(0),
    );
    assert!(decode_version(&descriptor, br#"{"id":"1.21.5"}"#).is_ok());
    for bytes in [
        br#"{"id":"other"}"#.as_slice(),
        br#"{"id":"1.21.5","inheritsFrom":"../outside"}"#,
        br#"{"id":"1.21.5","axialMaterialized":true}"#,
    ] {
        assert_eq!(
            decode_version(&descriptor, bytes),
            Err(CatalogError::Malformed)
        );
    }
}
