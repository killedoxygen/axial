//! Deterministic integration, not real-JVM gameplay or installed-package proof.
//! Every install source comes over loopback HTTP and must pass the normal
//! downloader, managed-runtime, publication, queue and launch owners.

use super::{
    DesktopServices, start_in_profile, start_profile_with_performance_test_inputs,
    start_profile_with_test_endpoints, transport,
};
use axial_minecraft::download::InstallTestEndpoints;
use axum::{
    Router,
    extract::{OriginalUri, State},
    http::{Method, StatusCode},
};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha512;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

const VERSION: &str = "axial-offline-fixture";
const COMPONENT: &str = "java-runtime-gamma";
const ASSET: &[u8] = b"real downloaded fixture asset";
const NATIVE: &[u8] = b"fixture native bytes, not an executable native library";
const PLAYER: &str = "FixturePlayer";
const INTERRUPTED_CHILD_PROFILE: &str = "AXIAL_TEST_INTERRUPTED_GAME_PROFILE";
const INTERRUPTED_CHILD_INSTANCE: &str = "AXIAL_TEST_INTERRUPTED_GAME_INSTANCE";
const INTERRUPTED_CHILD_INTENT: &str = "AXIAL_TEST_INTERRUPTED_GAME_INTENT";
const INTERRUPTED_CHILD_BENCHMARK: &str = "AXIAL_TEST_INTERRUPTED_GAME_BENCHMARK";
const INTERRUPTED_CLEANUP_PORT: &str = "AXIAL_TEST_INTERRUPTED_GAME_CLEANUP_PORT";
const INTERRUPTED_CLEANUP_TOKEN: &str = "AXIAL_TEST_INTERRUPTED_GAME_CLEANUP_TOKEN";
const INTERRUPTED_CHILD_EXIT: i32 = 75;
const EXTERNAL_CANARY: &[u8] = b"external user file survives launch and profile reopen";

fn configure_external(profile: &std::path::Path) -> (axial_app::library::LibraryId, PathBuf) {
    let admitted = super::admit_profile(profile).unwrap();
    let external = admitted.root.parent().unwrap().join("external");
    std::fs::create_dir(&external).unwrap();
    let external = std::fs::canonicalize(external).unwrap();
    std::fs::write(external.join("user-canary.bin"), EXTERNAL_CANARY).unwrap();
    let library_id = axial_app::library::LibraryId::new();
    std::fs::write(
        admitted.root.join("library.json"),
        serde_json::to_vec(&json!({
            "mode":"existing", "library_id":library_id.to_string(), "path":external,
        }))
        .unwrap(),
    )
    .unwrap();
    (library_id, external)
}

fn assert_external_library(
    services: &DesktopServices,
    profile: &std::path::Path,
    selection: &(axial_app::library::LibraryId, PathBuf),
) {
    let (id, external) = selection;
    let pin = services.library.admit().unwrap();
    assert_eq!(pin.library_id(), *id);
    assert_eq!(pin.read_projection().unwrap(), *external);
    assert_eq!(
        services.library.snapshot().current.unwrap().mode,
        axial_app::library::LibraryMode::Existing
    );
    assert_eq!(
        services
            .library
            .admit_application_root()
            .unwrap()
            .read_projection()
            .unwrap(),
        profile
    );
    let runtime = services.installs.runtime_cache().root();
    assert!(runtime.starts_with(profile));
    assert!(!runtime.starts_with(external));
    assert_eq!(
        std::fs::read(external.join("user-canary.bin")).unwrap(),
        EXTERNAL_CANARY
    );
}

fn sha1(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

fn archive(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(name, zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(bytes).unwrap();
    writer.finish().unwrap().into_inner()
}

fn java_relative_path() -> &'static str {
    if cfg!(target_os = "macos") {
        "jre.bundle/Contents/Home/bin/java"
    } else {
        "bin/java"
    }
}

fn native_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libfixture.dylib"
    } else {
        "libfixture.so"
    }
}

fn fake_java(natural_exit: bool) -> (Vec<u8>, Vec<u8>) {
    let python = std::process::Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("the existing acceptance fake Java fixture requires Python 3");
    assert!(
        python.status.success(),
        "Python 3 is required for the acceptance fixture"
    );
    let python = String::from_utf8(python.stdout).unwrap();
    let python = python.trim().replace('\'', "'\\''");
    let launcher =
        format!("#!/bin/sh\nexec '{python}' \"$(dirname \"$0\")/fake_java.py\" \"$@\"\n");
    let mut config = json!({
        "java_version":"17.0.12", "arch":std::env::consts::ARCH,
        "events":[
            {"stream":"stdout","text":"LWJGL Version: fixture\n"},
            {"stream":"stderr","text":"Fixture standard error\n"}
        ],
        "stall_ms":30000, "descendant_depth":1, "descendant_stall_ms":30000,
        "descendants_ignore_sigterm":true, "report_lifecycle":true
    });
    if natural_exit {
        config["events"][0]["delay_ms"] = json!(50);
        config["stall_ms"] = json!(100);
        config["descendant_depth"] = json!(0);
        config["exit_code"] = json!(0);
    }
    // Instrument only this downloaded fixture, never process-global test env.
    // Command checks execute inside the actual owned child after Java probing.
    let prelude = format!(
        r#"import json, os, pathlib, sys
os.environ["AXIAL_FAKE_JAVA"] = {config_literal}
if ("net.minecraft.client.main.Main" in sys.argv[1:] or "--fixture-descendant" in sys.argv[1:]) and os.environ.get("{INTERRUPTED_CLEANUP_PORT}"):
    import socket, threading
    cleanup = socket.create_connection(("127.0.0.1", int(os.environ["{INTERRUPTED_CLEANUP_PORT}"])), timeout=5)
    cleanup.settimeout(None)
    cleanup.sendall((os.environ["{INTERRUPTED_CLEANUP_TOKEN}"] + " " + str(os.getpid()) + "\n").encode())
    def await_fixture_cleanup():
        try:
            while cleanup.recv(1) == b"?":
                cleanup.sendall(b"!")
        except OSError:
            pass
        os._exit(0)
    threading.Thread(target=await_fixture_cleanup, daemon=True).start()
if "net.minecraft.client.main.Main" in sys.argv[1:]:
    args = sys.argv[1:]
    def value(flag):
        return args[args.index(flag) + 1]
    assert value("--username") == "{PLAYER}"
    assert value("--version") == "{VERSION}"
    assert pathlib.Path(value("--gameDir")).resolve() == pathlib.Path.cwd().resolve()
    assert value("--assetIndex") == "fixture-assets"
    asset = pathlib.Path(value("--assetsDir")) / "objects" / "{asset_prefix}" / "{asset_hash}"
    assert asset.read_bytes() == {asset_literal}
    classpath = [pathlib.Path(part) for part in value("-cp").split(os.pathsep)]
    assert all(path.is_file() for path in classpath)
    assert any(path.name == "{VERSION}.jar" for path in classpath)
    assert any(path.name == "fixture-1.0.jar" for path in classpath)
    native = next(arg.split("=", 1)[1] for arg in args if arg.startswith("-Djava.library.path="))
    assert (pathlib.Path(native) / "{native}").read_bytes() == {native_literal}
    assert "net.minecraft.client.main.Main" in args
    heap_mb = int(next(arg[4:-1] for arg in args if arg.startswith("-Xmx") and arg.endswith("M")))
    print("Fixture heap MiB " + str(heap_mb), flush=True)
    print("Fixture command elements " + str(len(sys.argv)), flush=True)
    print("Fixture command validated", flush=True)
"#,
        config_literal = serde_json::to_string(&config.to_string()).unwrap(),
        asset_prefix = &sha1(ASSET)[..2],
        asset_hash = sha1(ASSET),
        asset_literal = format!(
            "{}.encode()",
            serde_json::to_string(std::str::from_utf8(ASSET).unwrap()).unwrap()
        ),
        native = native_name(),
        native_literal = format!(
            "{}.encode()",
            serde_json::to_string(std::str::from_utf8(NATIVE).unwrap()).unwrap()
        ),
    );
    let helper = format!(
        "{prelude}\n{}",
        include_str!("../../../acceptance/support/fake_java.py")
    );
    (launcher.into_bytes(), helper.into_bytes())
}

fn fixture_output(arguments: &[&str]) -> std::process::Output {
    std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../acceptance/support/fake_java.py"
        ))
        .args(arguments)
        .env(
            "AXIAL_FAKE_JAVA",
            json!({
                "events":[{"text":"fixture game output\n"}],
                "exit_code":19
            })
            .to_string(),
        )
        .output()
        .expect("Python 3 is required for the acceptance fixture")
}

#[test]
fn fake_java_game_version_argument_runs_the_process_scenario() {
    for arguments in [
        vec!["net.minecraft.client.main.Main", "--version", VERSION],
        vec![
            "-cp",
            "fixture.jar",
            "net.minecraft.client.main.Main",
            "--version",
            VERSION,
        ],
        vec![
            "--class-path",
            "fixture.jar",
            "net.minecraft.client.main.Main",
            "-version",
        ],
        vec!["-jar", "fixture.jar", "--version", VERSION],
    ] {
        let output = fixture_output(&arguments);
        assert_eq!(output.status.code(), Some(19), "{arguments:?}: {output:?}");
        assert_eq!(output.stdout, b"fixture game output\n", "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
    }
}

#[test]
fn fake_java_launcher_version_options_remain_probes() {
    for arguments in [
        vec!["-version"],
        vec!["--version"],
        vec!["-XshowSettings:properties", "-version"],
        vec!["-cp", "fixture.jar", "-version"],
    ] {
        let output = fixture_output(&arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        let version = "openjdk version \"17.0.12\"";
        if arguments.contains(&"--version") {
            assert!(stdout.contains(version));
            assert!(stderr.is_empty());
        } else {
            assert!(stdout.is_empty());
            assert!(stderr.contains(version));
        }
        if arguments.contains(&"-XshowSettings:properties") {
            assert!(stderr.contains("java.version = 17.0.12"));
            assert!(stderr.contains("java.vendor = Fixture OpenJDK"));
            assert!(stderr.contains("os.arch = amd64"));
        }
    }
}

fn provider_routes(
    base: &str,
    corrupt_client: bool,
    natural_exit: bool,
) -> BTreeMap<String, Vec<u8>> {
    let mut routes = BTreeMap::new();
    let mut source = |path: &str, bytes: Vec<u8>| {
        let descriptor =
            json!({"url":format!("{base}{path}"),"size":bytes.len(),"sha1":sha1(&bytes)});
        assert!(routes.insert(format!("GET {path}"), bytes).is_none());
        descriptor
    };
    let client = source(
        "/artifacts/client.jar",
        archive("fixture-client.txt", b"fixture client"),
    );
    let library = source(
        "/artifacts/library.jar",
        archive("fixture-library.txt", b"fixture library"),
    );
    let native = source("/artifacts/natives.jar", archive(native_name(), NATIVE));
    let log = source(
        "/artifacts/log.xml",
        b"<Configuration status=\"WARN\"/>".to_vec(),
    );
    let asset_hash = sha1(ASSET);
    source(
        &format!("/assets/objects/{}/{asset_hash}", &asset_hash[..2]),
        ASSET.to_vec(),
    );
    let asset_index = source(
        "/assets/index.json",
        serde_json::to_vec(&json!({
            "objects":{"fixture/asset.txt":{"hash":asset_hash,"size":ASSET.len()}}
        }))
        .unwrap(),
    );
    let (java, helper) = fake_java(natural_exit);
    let java = source("/java-runtime/java", java);
    let helper = source("/java-runtime/fake_java.py", helper);
    let helper_path = java_relative_path().replace("/java", "/fake_java.py");
    let runtime = source(
        "/java-runtime/component.json",
        serde_json::to_vec(&json!({"files":{
            (java_relative_path()):{"type":"file","executable":true,"downloads":{"raw":java}},
            (helper_path):{"type":"file","downloads":{"raw":helper}}
        }}))
        .unwrap(),
    );
    source(
        "/java-runtime/all.json",
        serde_json::to_vec(&json!({
            "mac-os-arm64":{(COMPONENT):[{"manifest":runtime}]},
            "mac-os":{(COMPONENT):[{"manifest":runtime}]},
            "linux":{(COMPONENT):[{"manifest":runtime}]},
            "linux-i386":{(COMPONENT):[{"manifest":runtime}]}
        }))
        .unwrap(),
    );
    let classifier = if cfg!(target_os = "macos") {
        "natives-macos"
    } else {
        "natives-linux"
    };
    let library_path = "org/axial/fixture/1.0/fixture-1.0.jar";
    let native_path = format!("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-{classifier}.jar");
    let mut library = library;
    library["path"] = json!(library_path);
    let mut native = native;
    native["path"] = json!(native_path);
    let mut log = log;
    log["id"] = json!("fixture-log.xml");
    let mut asset_index = asset_index;
    asset_index["id"] = json!("fixture-assets");
    asset_index["totalSize"] = json!(ASSET.len());
    let version = source("/versions/fixture.json", serde_json::to_vec(&json!({
        "id":VERSION,"type":"release","mainClass":"net.minecraft.client.main.Main",
        "time":"2024-01-01T00:00:00Z","releaseTime":"2024-01-01T00:00:00Z",
        "javaVersion":{"component":COMPONENT,"majorVersion":17},
        "downloads":{"client":client},"assetIndex":asset_index,"assets":"fixture-assets",
        "logging":{"client":{"file":log,"argument":"-Dlog4j.configurationFile=${path}","type":"log4j2-xml"}},
        "arguments":{
            "jvm":["-Djava.library.path=${natives_directory}","-cp","${classpath}"],
            "game":["--username","${auth_player_name}","--version","${version_name}",
                "--gameDir","${game_directory}","--assetsDir","${assets_root}",
                "--assetIndex","${assets_index_name}","--uuid","${auth_uuid}",
                "--accessToken","${auth_access_token}","--userType","${user_type}"]
        },
        "libraries":[
            {"name":"org.axial:fixture:1.0","downloads":{"artifact":library}},
            {"name":"org.lwjgl:lwjgl:3.3.3","natives":{"osx":classifier,"linux":classifier},
                "downloads":{"classifiers":{(classifier):native}}}
        ]
    })).unwrap());
    source(
        "/version_manifest_v2.json",
        serde_json::to_vec(&json!({
            "latest":{"release":VERSION,"snapshot":VERSION},
            "versions":[{"id":VERSION,"type":"release","url":version["url"],"sha1":version["sha1"],
                "time":"2024-01-01T00:00:00Z","releaseTime":"2024-01-01T00:00:00Z"}]
        }))
        .unwrap(),
    );
    if corrupt_client {
        // Change one byte after all parent metadata has committed to its digest.
        routes.get_mut("GET /artifacts/client.jar").unwrap()[0] ^= 1;
    }
    routes
}

#[derive(Clone)]
struct ProviderState {
    routes: Arc<BTreeMap<String, Vec<u8>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

struct Provider {
    base: String,
    state: ProviderState,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Provider {
    async fn start(corrupt_client: bool) -> Self {
        Self::start_with_java_exit(corrupt_client, false).await
    }

    async fn start_with_java_exit(corrupt_client: bool, natural_exit: bool) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = ProviderState {
            routes: Arc::new(provider_routes(&base, corrupt_client, natural_exit)),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let router = Router::new()
            .fallback(
                |State(state): State<ProviderState>,
                 method: Method,
                 OriginalUri(uri): OriginalUri| async move {
                    let key = format!("{method} {uri}");
                    state.requests.lock().unwrap().push(key.clone());
                    match state.routes.get(&key) {
                        Some(bytes) => (StatusCode::OK, bytes.clone()),
                        None => (
                            StatusCode::NOT_IMPLEMENTED,
                            b"unmatched fixture request".to_vec(),
                        ),
                    }
                },
            )
            .with_state(state.clone());
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            base,
            state,
            stop: Some(stop),
            task,
        }
    }

    fn endpoints(&self) -> InstallTestEndpoints {
        InstallTestEndpoints::from_loopback_base_url(&self.base).unwrap()
    }

    fn assert_requests(&self, all: bool) {
        let requests: BTreeSet<_> = self
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        let expected: BTreeSet<_> = self.state.routes.keys().cloned().collect();
        assert!(
            requests.is_subset(&expected),
            "unexpected provider requests: {requests:?}"
        );
        if all {
            assert_eq!(
                requests, expected,
                "every install artifact must be acquired"
            );
        }
    }

    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct PerformanceProvider {
    base: String,
    routes: Arc<BTreeMap<String, Vec<u8>>>,
    requests: Arc<Mutex<Vec<String>>>,
    hold: tokio::sync::watch::Sender<bool>,
    held: Arc<tokio::sync::Notify>,
    changed_graph: Arc<std::sync::atomic::AtomicBool>,
    task: JoinHandle<()>,
}

impl PerformanceProvider {
    async fn start() -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let make_routes = |mod_version: &str| {
            let mut routes = BTreeMap::new();
            for artifact in axial_performance::builtin_manifest().unwrap().artifacts {
                let project = artifact.source.project_id;
                let bytes = archive(
                    "fabric.mod.json",
                    &serde_json::to_vec(&json!({
                        "schemaVersion":1,"id":artifact.id.replace('-', "_"),
                        "version":mod_version,"environment":"client"
                    }))
                    .unwrap(),
                );
                let filename = format!("fixture-{project}.jar");
                let version = json!({
                    "id":project,"project_id":project,
                    "name":"Fixture","version_number":"1.0.0","version_type":"release",
                    "game_versions":["1.21.4"],"loaders":["fabric"],"dependencies":[],
                    "files":[{"hashes":{"sha512":format!("{:x}", Sha512::digest(&bytes))},
                        "url":format!("{base}/artifacts/{filename}"),"filename":filename,
                        "primary":true,"size":bytes.len()}]
                });
                routes.insert(format!("/artifacts/{filename}"), bytes);
                routes.insert(
                    format!("/v2/project/{project}"),
                    serde_json::to_vec(
                        &json!({"id":project,"title":artifact.id,"project_type":"mod"}),
                    )
                    .unwrap(),
                );
                routes.insert(
                    format!("/v2/project/{project}/version"),
                    serde_json::to_vec(&json!([version])).unwrap(),
                );
            }
            routes
        };
        let routes = Arc::new(make_routes("1.0.0"));
        let changed_routes = Arc::new(make_routes("1.0.1"));
        let changed_graph = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (hold, gate) = tokio::sync::watch::channel(false);
        let held = Arc::new(tokio::sync::Notify::new());
        let router = Router::new().fallback({
            let routes = routes.clone();
            let requests = requests.clone();
            let held = held.clone();
            let changed_graph = changed_graph.clone();
            move |method: Method, OriginalUri(uri): OriginalUri| {
                let routes = routes.clone();
                let changed_routes = changed_routes.clone();
                let changed_graph = changed_graph.clone();
                let requests = requests.clone();
                let held = held.clone();
                let mut gate = gate.clone();
                async move {
                    requests.lock().unwrap().push(format!("{method} {uri}"));
                    let should_hold = *gate.borrow();
                    if should_hold {
                        held.notify_one();
                        if tokio::time::timeout(Duration::from_secs(30), async {
                            loop {
                                let should_hold = *gate.borrow_and_update();
                                if !should_hold {
                                    break;
                                }
                                if gate.changed().await.is_err() {
                                    break;
                                }
                            }
                        })
                        .await
                        .is_err()
                        {
                            return (
                                StatusCode::GATEWAY_TIMEOUT,
                                b"fixture gate deadline".to_vec(),
                            );
                        }
                    }
                    let routes = if changed_graph.load(std::sync::atomic::Ordering::SeqCst) {
                        changed_routes
                    } else {
                        routes
                    };
                    let bytes = if method == Method::GET && uri.path() == "/v2/projects" {
                        url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                            .find(|(key, _)| key == "ids")
                            .and_then(|(_, ids)| serde_json::from_str::<Vec<String>>(&ids).ok())
                            .and_then(|ids| {
                                ids.into_iter()
                                    .map(|id| {
                                        routes.get(&format!("/v2/project/{id}")).and_then(|bytes| {
                                            serde_json::from_slice::<Value>(bytes).ok()
                                        })
                                    })
                                    .collect::<Option<Vec<_>>>()
                            })
                            .map(|projects| serde_json::to_vec(&projects).unwrap())
                    } else if method == Method::GET {
                        routes.get(uri.path()).cloned()
                    } else {
                        None
                    };
                    match bytes {
                        Some(bytes) => (StatusCode::OK, bytes),
                        None => (
                            StatusCode::NOT_IMPLEMENTED,
                            b"unmatched performance fixture request".to_vec(),
                        ),
                    }
                }
            }
        });
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            base,
            routes,
            requests,
            hold,
            held,
            changed_graph,
            task,
        }
    }

    fn artifact_requests(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for request in self.requests.lock().unwrap().iter() {
            if let Some(path) = request.strip_prefix("GET /artifacts/") {
                *counts.entry(path.to_owned()).or_default() += 1;
            }
        }
        counts
    }
}

impl Drop for PerformanceProvider {
    fn drop(&mut self) {
        self.hold.send_replace(false);
        self.task.abort();
    }
}

fn performance_fixture_transfers(base: &str) -> axial_performance::ManagedArtifactTransferResolver {
    use axial_minecraft::download::{
        RetryPolicy, TransferClient, TransferClientConfig, TransferOrigin,
    };
    let expected = url::Url::parse(base).unwrap().origin();
    axial_performance::ManagedArtifactTransferResolver::new(
        move |url| {
            let expected = expected.clone();
            async move {
                if url.origin() != expected {
                    return Err(std::io::Error::other("artifact escaped the fixture origin"));
                }
                let origin = TransferOrigin::from_loopback_http_for_test_support(&url)
                    .map_err(std::io::Error::other)?;
                let config = TransferClientConfig::bounded(
                    Duration::from_secs(2),
                    Duration::from_secs(35),
                    Duration::from_secs(40),
                    vec![origin],
                )
                .map_err(std::io::Error::other)?;
                TransferClient::build(config).map_err(std::io::Error::other)
            }
        },
        RetryPolicy::none(),
    )
}

struct Api {
    client: reqwest::Client,
    base: String,
    capability: String,
}

impl Api {
    fn new(services: &DesktopServices) -> Self {
        let bootstrap = services.server.bootstrap();
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(45))
                .build()
                .unwrap(),
            base: bootstrap.base_url,
            capability: bootstrap.capability,
        }
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header(transport::CAPABILITY_HEADER, &self.capability);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "{path}: {status}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        self.request(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    async fn events(&self, path: &str) -> Events {
        let ticket = self
            .post(
                "/api/v1/transport/tickets",
                json!({"audience":"stream","target":path}),
            )
            .await;
        let response = self
            .client
            .get(format!(
                "{}{path}?axial_ticket={}",
                self.base,
                ticket["ticket"].as_str().unwrap()
            ))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        Events {
            response,
            buffered: Vec::new(),
        }
    }
}

struct Events {
    response: reqwest::Response,
    buffered: Vec<u8>,
}

impl Events {
    async fn next(&mut self) -> (String, Value) {
        loop {
            if let Some(end) = self.buffered.windows(2).position(|bytes| bytes == b"\n\n") {
                let frame: Vec<_> = self.buffered.drain(..end + 2).collect();
                let frame = String::from_utf8(frame).unwrap();
                let event = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("event:"))
                    .unwrap_or("message")
                    .trim();
                if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data:")) {
                    return (event.to_owned(), serde_json::from_str(data.trim()).unwrap());
                }
                continue;
            }
            self.buffered.extend_from_slice(
                &self
                    .response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("stream ended before terminal state"),
            );
            assert!(self.buffered.len() <= 1 << 20, "bounded fixture stream");
        }
    }
}

async fn install_terminal(api: &Api, start: &Value) -> Value {
    let id = start["started_install"]["install_id"]
        .as_str()
        .expect("accepted durable install identity");
    let mut events = api.events(&format!("/api/v1/install/{id}/events")).await;
    let terminal = tokio::time::timeout(Duration::from_secs(40), async {
        let mut previous = 0;
        loop {
            let (_, event) = events.next().await;
            let revision = event["revision"].as_u64().unwrap();
            assert!(revision >= previous);
            previous = revision;
            if event["value"]["done"] == true {
                break event["value"].clone();
            }
        }
    })
    .await
    .expect("install must reach a real settled terminal state");
    assert_eq!(terminal["view_model"]["terminal"], true, "{terminal}");
    assert_eq!(
        api.get(&format!("/api/v1/install/{id}/status")).await,
        terminal
    );
    terminal
}

async fn assert_installed(api: &Api, expected: bool) {
    let versions = api.get("/api/v1/versions").await;
    let installed = versions["versions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|version| {
            version["id"] == VERSION
                && version["installed"] == true
                && version["launchable"] == true
        });
    assert_eq!(installed, expected, "{versions}");
}

async fn wait_launchable(api: &Api, instance: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = api
                .get(&format!("/api/v1/launch/preflight/{instance}"))
                .await;
            if status["launchable"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("installed instance must become launchable");
}

async fn launch_and_stop(api: &Api, instance: &str) -> String {
    wait_launchable(api, instance).await;
    let request = json!({"instance_id":instance,"intent_key":uuid::Uuid::new_v4().to_string()});
    let launched = api.post("/api/v1/launch", request.clone()).await;
    let id = launched["session_id"].as_str().unwrap().to_owned();
    assert_eq!(api.post("/api/v1/launch", request).await["session_id"], id);
    observe_and_stop_session(api, &id).await;
    id
}

async fn observe_and_stop_session(api: &Api, id: &str) -> BTreeSet<u64> {
    let (mut events, processes) = observe_running_session(api, id).await;
    api.post(&format!("/api/v1/launch/{id}/kill"), json!({}))
        .await;
    let terminal = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let (event, value) = events.next().await;
            if event == "status" && value["phase"] == "exited" {
                break value;
            }
        }
    })
    .await
    .expect("stop must settle process tree and drain output");
    assert_eq!(terminal["tree_settled"], true, "{terminal}");
    assert_eq!(terminal["output_drained"], true, "{terminal}");
    assert_eq!(terminal["process_alive"], false, "{terminal}");
    assert_eq!(terminal["boot_observed"], true, "{terminal}");
    assert_eq!(terminal["outcome"]["kind"], "stopped", "{terminal}");
    assert_eq!(
        terminal["outcome"]["reason"], "launcher_stopped",
        "{terminal}"
    );
    assert_eq!(
        api.get(&format!("/api/v1/launch/{id}/status")).await,
        terminal
    );
    processes
}

async fn observe_running_session(api: &Api, id: &str) -> (Events, BTreeSet<u64>) {
    let mut events = api.events(&format!("/api/v1/launch/{id}/events")).await;
    let mut stdout = false;
    let mut stderr = false;
    let mut command = false;
    let mut running = false;
    let mut processes = BTreeSet::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !(stdout && stderr && command && running && processes.len() == 2) {
            let (event, value) = events.next().await;
            if event == "status" {
                assert_ne!(
                    value["phase"], "exited",
                    "fake Java exited before assertions: {value}"
                );
                running |= value["phase"] == "running" && value["process_alive"] == true;
            } else if event == "log" {
                let text = value["text"].as_str().unwrap();
                stdout |= value["source"] == "stdout" && text.contains("LWJGL Version: fixture");
                stderr |= value["source"] == "stderr" && text.contains("Fixture standard error");
                command |=
                    value["source"] == "stdout" && text.contains("Fixture command validated");
                if let Some(lifecycle) = text.strip_prefix("AXIAL_FAKE_JAVA ") {
                    let lifecycle: Value = serde_json::from_str(lifecycle).unwrap();
                    if lifecycle["event"] == "started" {
                        processes.insert(lifecycle["pid"].as_u64().unwrap());
                    }
                }
            }
        }
    })
    .await
    .expect("real owned Java and its descendant must emit status and both output streams");
    (events, processes)
}

const SETTLED_CHILD_PROFILE: &str = "AXIAL_TEST_SETTLED_REPORT_PROFILE";
const SETTLED_CHILD_INSTANCE: &str = "AXIAL_TEST_SETTLED_REPORT_INSTANCE";
const SETTLED_CHILD_INTENT: &str = "AXIAL_TEST_SETTLED_REPORT_INTENT";
const SETTLED_CHILD_EXIT: i32 = 74;
const DRIVER_CHILD_PROFILE: &str = "AXIAL_TEST_DRIVER_RESTART_PROFILE";
const DRIVER_CHILD_INSTANCE: &str = "AXIAL_TEST_DRIVER_RESTART_INSTANCE";
const DRIVER_CHILD_EXIT: i32 = 76;
const RESTART_SUITE: &str = "automatic-restart-suite";

fn fixture_child_log_tail(output: &mut std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};
    output
        .seek(SeekFrom::Start(
            output.metadata().unwrap().len().saturating_sub(65536),
        ))
        .unwrap();
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes).unwrap();
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .take(30)
        .collect::<Vec<_>>()
        .join("\n")
}

async fn assert_fixture_child_exit(
    mut child: tokio::process::Child,
    output: &mut std::fs::File,
    expected: i32,
) {
    let result = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let tail = fixture_child_log_tail(output);
    assert!(result.is_ok(), "fixture helper timed out: {tail:?}");
    assert_eq!(result.unwrap().unwrap().code(), Some(expected), "{tail:?}");
}

const PREPARED_PROFILE: &str = "AXIAL_TEST_PREPARED_PERFORMANCE_PROFILE";
const PREPARED_INSTANCE: &str = "AXIAL_TEST_PREPARED_PERFORMANCE_INSTANCE";
const PREPARED_PROVIDER: &str = "AXIAL_TEST_PREPARED_PERFORMANCE_PROVIDER";
const PREPARED_QUEUED: &str = "AXIAL_TEST_PREPARED_PERFORMANCE_QUEUED";
const PREPARED_ACTION: &str = "AXIAL_TEST_PREPARED_PERFORMANCE_ACTION";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_prepared_performance_apply_resumes_queued_without_blocking_startup() {
    prepared_performance_restart_journey(true, "apply", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_prepared_performance_apply_resumes_synchronous_without_blocking_startup() {
    prepared_performance_restart_journey(false, "apply", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_prepared_performance_reapply_resumes_synchronous_without_blocking_startup() {
    prepared_performance_restart_journey(false, "reapply", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_prepared_performance_rejects_changed_provider_graph_without_file_effects() {
    prepared_performance_restart_journey(true, "apply", true).await;
}

async fn prepared_performance_restart_journey(queued: bool, action: &str, changed_graph: bool) {
    use axial_app::{
        instances::create::{CreateInstanceRequest, CreateTarget},
        storage::{MetadataStore, StorageError},
    };
    use axial_minecraft::loaders::LoaderComponentId;
    use std::process::Stdio;

    let temporary = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let canary = archive(
        "fabric.mod.json",
        br#"{"schemaVersion":1,"id":"fixture_user","version":"1.0.0"}"#,
    );

    let provider = PerformanceProvider::start().await;
    let services = start_profile_with_performance_test_inputs(
        profile.clone(),
        format!("{}/v2", provider.base),
        performance_fixture_transfers(&provider.base),
    )
    .await
    .unwrap();
    let api = Api::new(&services);
    let target =
        CreateTarget::loader_for_tests(LoaderComponentId::Fabric, "1.21.4", "0.16.9").unwrap();
    let created = services
        .instances
        .create(
            CreateInstanceRequest {
                name: "Prepared recovery".into(),
                selection_id: target.selection_id().to_owned(),
                ..Default::default()
            },
            target,
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let instance = created.id.to_string();
    assert_eq!(created.loader_key, "fabric");
    assert_eq!(created.minecraft_version, "1.21.4");
    api.request(
        reqwest::Method::PUT,
        &format!("/api/v1/instances/{instance}"),
        Some(json!({"performance_mode":"managed"})),
    )
    .await;
    let mods = services
        .library
        .admit()
        .unwrap()
        .read_projection()
        .unwrap()
        .join("instances")
        .join(&instance)
        .join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    std::fs::write(mods.join("user.jar"), &canary).unwrap();
    services.server.shutdown().await.unwrap();
    drop(services);

    let mut output = tempfile::tempfile_in(temporary.path()).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "offline_journey_tests::prepared_performance_crash_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(PREPARED_PROFILE, &profile)
        .env(PREPARED_INSTANCE, &instance)
        .env(PREPARED_PROVIDER, &provider.base)
        .env(PREPARED_QUEUED, if queued { "1" } else { "0" })
        .env(PREPARED_ACTION, action)
        .env("AXIAL_PERFORMANCE_PREPARED_CRASH", "1")
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output.try_clone().unwrap()))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    if exit.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let requests: Vec<_> = provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .take(20)
        .cloned()
        .collect();
    let tail = fixture_child_log_tail(&mut output);
    assert_eq!(
        exit.ok()
            .and_then(Result::ok)
            .and_then(|status| status.code()),
        Some(42),
        "Prepared helper did not reach its checkpoint: {tail:?}; fixture request tail: {requests:?}"
    );
    let (command, prepared) = MetadataStore::open(profile.join("metadata.sqlite"))
        .unwrap()
        .read(|db| -> Result<_, StorageError> {
            assert_eq!(
                db.query_row("SELECT count(*) FROM performance_commands", [], |row| row
                    .get::<_, i64>(
                    0
                ))?,
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM performance_operations", [], |row| row
                    .get::<_, i64>(0))?,
                1
            );
            let command: String = db.query_row(
                "SELECT id FROM performance_commands WHERE instance_id=?1",
                [&instance],
                |row| row.get(0),
            )?;
            let (operation, bytes): (String, Vec<u8>) = db.query_row(
                "SELECT operation_id,payload FROM performance_operations WHERE instance_id=?1",
                [&instance],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(
                operation, command,
                "Prepared retains the accepted command, including synchronous entry"
            );
            Ok((command, serde_json::from_slice::<Value>(&bytes).unwrap()))
        })
        .unwrap();
    assert_eq!(prepared["operation_id"], command);
    assert_eq!(prepared["instance_id"], instance);
    assert_eq!(prepared["target_effect_started"], false);
    assert!(prepared["before"].is_null());
    assert!(prepared["result"].is_null());
    assert!(!prepared["directory_receipt"].as_str().unwrap().is_empty());
    let expected = prepared["expected"]["artifacts"].as_array().unwrap();
    assert!(!expected.is_empty());
    assert!(
        provider.artifact_requests().is_empty(),
        "Prepared cannot transfer artifacts before the crash"
    );
    assert_eq!(std::fs::read(mods.join("user.jar")).unwrap(), canary);
    assert_eq!(std::fs::read_dir(&mods).unwrap().count(), 1);

    provider.hold.send_replace(true);
    let started = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            start_profile_with_performance_test_inputs(
                profile.clone(),
                format!("{}/v2", provider.base),
                performance_fixture_transfers(&provider.base),
            ),
            provider.held.notified(),
        )
    })
    .await;
    if started.is_err() {
        provider.hold.send_replace(false);
    }
    let (services, ()) =
        started.expect("startup must return while resumed provider I/O is still held");
    let services = services.unwrap();
    let api = Api::new(&services);
    let operation_path = format!("/api/v1/performance/operations/{command}");
    tokio::time::timeout(Duration::from_secs(3), async {
        api.get("/api/v1/config").await;
        let operation = api.get(&operation_path).await;
        assert_eq!(operation["id"], command);
        assert_eq!(operation["instance_id"], instance);
        assert_eq!(operation["action"], action);
        assert_eq!(operation["view_model"]["is_terminal"], false, "{operation}");
        assert_eq!(
            api.get(&format!(
                "/api/v1/performance/instances/{instance}/operation"
            ))
            .await["operation"],
            operation
        );
        for (method, path, body) in [
            (
                reqwest::Method::PUT,
                format!("/api/v1/instances/{instance}"),
                json!({"name":"Must remain reserved"}),
            ),
            (
                reqwest::Method::DELETE,
                format!("/api/v1/instances/{instance}?keep_files=true"),
                Value::Null,
            ),
        ] {
            let response = api
                .client
                .request(method, format!("{}{path}", api.base))
                .header(transport::CAPABILITY_HEADER, &api.capability)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({
                    "error":axial_app::instances::model::InstanceError::Busy.to_string()
                })
            );
        }
    })
    .await
    .expect("held recovery must leave unrelated reads and Busy refusals responsive");
    assert!(provider.artifact_requests().is_empty());
    assert_eq!(std::fs::read(mods.join("user.jar")).unwrap(), canary);
    provider
        .changed_graph
        .store(changed_graph, std::sync::atomic::Ordering::SeqCst);
    provider.hold.send_replace(false);
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let operation = api.get(&operation_path).await;
            if operation["view_model"]["is_terminal"] == true && services.tasks.status().is_idle() {
                break operation;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the original accepted operation must finish after the provider is released");
    assert_eq!(
        completed["state"],
        if changed_graph { "failed" } else { "complete" },
        "{completed}"
    );
    assert_eq!(completed["id"], command);
    assert_eq!(
        services
            .instances
            .registry()
            .storage()
            .read(|db| -> Result<i64, StorageError> {
                Ok(
                    db.query_row("SELECT count(*) FROM performance_operations", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap(),
        0
    );
    let health = api
        .get(&format!(
            "/api/v1/performance/health?instance_id={instance}"
        ))
        .await;
    let transferred = provider.artifact_requests();
    if changed_graph {
        assert!(
            transferred.is_empty(),
            "a newly resolved graph cannot replace the accepted Prepared graph"
        );
        assert!(health["state"].is_null(), "{health}");
        assert_eq!(health["view_model"]["state_id"], "disabled");
        assert_eq!(std::fs::read_dir(&mods).unwrap().count(), 1);
        api.request(
            reqwest::Method::PUT,
            &format!("/api/v1/instances/{instance}"),
            Some(json!({"name":created.name})),
        )
        .await;
    } else {
        assert_eq!(health["view_model"]["state_id"], "healthy", "{health}");
        assert_eq!(
            health["state"]["graph_sha512"],
            prepared["expected"]["graph_sha512"]
        );
        assert_eq!(
            health["state"]["installed_mods"].as_array().unwrap().len(),
            expected.len()
        );
        assert_eq!(transferred.len(), expected.len());
        for artifact in expected {
            let filename = artifact["filename"].as_str().unwrap();
            assert_eq!(transferred.get(filename), Some(&1));
            let bytes = std::fs::read(mods.join(filename)).unwrap();
            assert_eq!(bytes, provider.routes[&format!("/artifacts/{filename}")]);
            assert_eq!(format!("{:x}", Sha512::digest(&bytes)), artifact["sha512"]);
        }
    }
    services.server.shutdown().await.unwrap();
    drop(services);
    let services = start_profile_with_performance_test_inputs(
        profile.clone(),
        format!("{}/v2", provider.base),
        performance_fixture_transfers(&provider.base),
    )
    .await
    .unwrap();
    let api = Api::new(&services);
    assert_eq!(api.get(&operation_path).await, completed);
    assert_eq!(
        api.get(&format!(
            "/api/v1/performance/health?instance_id={instance}"
        ))
        .await,
        health
    );
    services.server.shutdown().await.unwrap();
    assert_eq!(
        provider.artifact_requests(),
        transferred,
        "cold startup cannot replay artifact effects"
    );
    services
        .instances
        .registry()
        .storage()
        .read(|db| -> Result<(), StorageError> {
            assert_eq!(
                db.query_row("SELECT id FROM performance_commands", [], |row| row
                    .get::<_, String>(0))?,
                command
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM performance_commands", [], |row| row
                    .get::<_, i64>(
                    0
                ))?,
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM performance_operations", [], |row| row
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(std::fs::read(mods.join("user.jar")).unwrap(), canary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess helper for the actual Prepared Performance boundary"]
async fn prepared_performance_crash_helper() {
    let Some(profile) = std::env::var_os(PREPARED_PROFILE) else {
        return;
    };
    let profile = PathBuf::from(profile);
    assert_eq!(profile.canonicalize().unwrap(), profile);
    assert!(std::env::var_os("AXIAL_PERFORMANCE_PREPARED_CRASH").is_some());
    let base = std::env::var(PREPARED_PROVIDER).unwrap();
    let instance = std::env::var(PREPARED_INSTANCE).unwrap();
    let queued = std::env::var(PREPARED_QUEUED).unwrap() == "1";
    let action = std::env::var(PREPARED_ACTION).unwrap();
    assert!(matches!(action.as_str(), "apply" | "reapply"));
    let services = start_profile_with_performance_test_inputs(
        profile,
        format!("{base}/v2"),
        performance_fixture_transfers(&base),
    )
    .await
    .unwrap();
    let api = Api::new(&services);
    let accepted = api
        .post(
            "/api/v1/performance/install",
            json!({
                "instance_id":instance,"action":action,"mode":"managed","queued":queued
            }),
        )
        .await;
    assert!(
        queued,
        "synchronous Apply returned without reaching its Prepared crash: {accepted}"
    );
    let command = accepted["install_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let operation = api
                .get(&format!("/api/v1/performance/operations/{command}"))
                .await;
            assert_ne!(
                operation["view_model"]["is_terminal"], true,
                "Apply did not reach Prepared: {operation}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Apply must reach its real Prepared crash hook");
}

fn unobserved_intent_payload(storage: &axial_app::storage::MetadataStore, intent: &str) -> Vec<u8> {
    use axial_app::storage::StorageError;
    storage.read(|db| -> Result<_, StorageError> {
        let (payload, settlement, acknowledged): (Vec<u8>, Option<Vec<u8>>, i64) = db.query_row(
            "SELECT payload,settlement,terminal_ack FROM launch_intents WHERE intent_key=?1 AND state='accepted'",
            [intent], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert!(settlement.is_none(), "test-owned cleanup is not application settlement");
        assert_eq!(acknowledged, 0);
        assert_eq!(db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get::<_, i64>(0))?, 1);
        assert_eq!(db.query_row("SELECT count(*) FROM launch_reports", [], |row| row.get::<_, i64>(0))?, 0);
        Ok(payload)
    }).unwrap()
}

async fn ping_fixture_channels(channels: &mut [tokio::net::TcpStream]) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(Duration::from_secs(2), async {
        for channel in channels {
            channel.write_all(b"?").await.unwrap();
            assert_eq!(channel.read_u8().await.unwrap(), b'!');
        }
    })
    .await
    .expect("both exact fixture processes must still be alive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_interrupted_launch_preserves_quit_and_restores_fences() {
    interrupted_launch_preservation_journey(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_external_interrupted_launch_preserves_quit_and_restores_fences() {
    interrupted_launch_preservation_journey(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_interrupted_benchmark_driver_does_not_repeat_unknown_session() {
    interrupted_launch_preservation_journey(true, false).await;
}

async fn interrupted_launch_preservation_journey(benchmark: bool, existing: bool) {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let external = existing.then(|| configure_external(&profile));
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    if let Some(selection) = &external {
        assert_external_library(&services, &profile, selection);
    }
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":0,"performance_mode":"vanilla","java_path_override":""
        })),
    )
    .await;
    let install = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    let terminal = install_terminal(&api, &install).await;
    assert_eq!(terminal["outcome"], "succeeded", "{terminal}");
    let created = api
        .post(
            "/api/v1/instances",
            json!({
                "name":"Interrupted fixture","selection_id":format!("vanilla|{VERSION}")
            }),
        )
        .await;
    let instance = created["id"].as_str().unwrap().to_owned();
    assert!(created["install_queue"].is_null(), "{created}");
    let native_root = services
        .library
        .admit()
        .unwrap()
        .read_projection()
        .unwrap()
        .join("cache/natives");
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    // Each process watches its own authenticated test connection. Dropping any
    // acquired connection (including during panic) exits only that fixture;
    // dropping the listener resets queued connections. The app never sees it.
    let cleanup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token = uuid::Uuid::new_v4().to_string();
    let intent = uuid::Uuid::new_v4().to_string();
    let mut output = tempfile::tempfile_in(temporary.path()).unwrap();
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "offline_journey_tests::interrupted_launch_crash_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(INTERRUPTED_CHILD_PROFILE, &profile)
        .env(INTERRUPTED_CHILD_INSTANCE, &instance)
        .env(INTERRUPTED_CHILD_INTENT, &intent)
        .env(
            INTERRUPTED_CHILD_BENCHMARK,
            if benchmark { "1" } else { "0" },
        )
        .env(
            INTERRUPTED_CLEANUP_PORT,
            cleanup.local_addr().unwrap().port().to_string(),
        )
        .env(INTERRUPTED_CLEANUP_TOKEN, &token)
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output.try_clone().unwrap()))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut channels = Vec::new();
    let mut processes = BTreeSet::new();
    let connected = tokio::time::timeout(Duration::from_secs(15), async {
        for _ in 0..2 {
            let (mut channel, peer) = cleanup.accept().await.unwrap();
            assert!(peer.ip().is_loopback());
            let mut header = Vec::new();
            loop {
                let byte = channel.read_u8().await.unwrap();
                if byte == b'\n' {
                    break;
                }
                header.push(byte);
                assert!(header.len() <= 64, "bounded fixture handshake");
            }
            let header = String::from_utf8(header).unwrap();
            let (received_token, pid) = header.split_once(' ').unwrap();
            assert!(received_token == token, "fixture channel authentication");
            assert!(processes.insert(pid.parse::<u64>().unwrap()));
            channels.push(channel);
        }
    })
    .await;
    if connected.is_err() {
        drop(channels);
        drop(cleanup);
        assert_fixture_child_exit(child, &mut output, INTERRUPTED_CHILD_EXIT).await;
        panic!("the launched fixture and descendant must both own cleanup channels");
    }
    assert_fixture_child_exit(child, &mut output, INTERRUPTED_CHILD_EXIT).await;
    ping_fixture_channels(&mut channels).await;
    let (payload, original_instance, interrupted_driver) = {
        let storage = Arc::new(
            axial_app::storage::MetadataStore::open(profile.join("metadata.sqlite")).unwrap(),
        );
        let interrupted_driver = benchmark.then(|| {
            storage
                .read(
                    |db| -> Result<(Value, Value), axial_app::storage::StorageError> {
                        let driver: Vec<u8> =
                            db.query_row("SELECT payload FROM benchmark_drivers", [], |row| {
                                row.get(0)
                            })?;
                        let suite: Vec<u8> =
                            db.query_row("SELECT payload FROM benchmark_suites", [], |row| {
                                row.get(0)
                            })?;
                        Ok((
                            serde_json::from_slice(&driver).unwrap(),
                            serde_json::from_slice(&suite).unwrap(),
                        ))
                    },
                )
                .unwrap()
        });
        let effective_intent = interrupted_driver
            .as_ref()
            .map_or(intent.as_str(), |(_, suite)| {
                suite["runs"][0]["launch_intent"].as_str().unwrap()
            });
        let payload = unobserved_intent_payload(&storage, effective_intent);
        let registry = axial_app::instances::directory::Registry::new(storage);
        (
            payload,
            registry.get_live(&instance.parse().unwrap()).unwrap(),
            interrupted_driver,
        )
    };
    let accepted: Value = serde_json::from_slice(&payload).unwrap();
    if let Some((id, _)) = &external {
        assert_eq!(accepted["binding"]["library_id"], id.to_string());
        assert_ne!(
            accepted["binding"]["library_root"],
            accepted["binding"]["application_root"]
        );
    }
    if !benchmark {
        assert_eq!(accepted["request"]["intent_key"], intent);
    }
    let intent = accepted["request"]["intent_key"].as_str().unwrap();
    let session = accepted["session_id"].as_str().unwrap();
    assert_eq!(accepted["request"]["instance_id"], instance);
    assert_eq!(accepted["request"]["intent_key"], intent);
    let natives = std::fs::read_dir(&native_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(natives.len(), 1);
    let native_files = [native_name(), ".axial-native-manifest.json"].map(|name| {
        let path = natives[0].join(name);
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    });
    assert_eq!(native_files[0].1, NATIVE);

    for _ in 0..2 {
        let reopened = start_in_profile(profile.clone(), None).await.unwrap();
        if let Some(selection) = &external {
            assert_external_library(&reopened, &profile, selection);
        }
        let api = Api::new(&reopened);
        ping_fixture_channels(&mut channels).await;
        let status = api.get(&format!("/api/v1/launch/intents/{intent}")).await;
        assert_eq!(status["state"], "interrupted");
        assert_eq!(status["code"], "interrupted");
        assert_eq!(status["session_id"], session);
        if let Some((driver, suite)) = &interrupted_driver {
            assert_eq!(driver["state"], "waiting");
            let driver_path = format!(
                "/api/v1/launch/benchmark/suite/drivers/{}",
                driver["id"].as_str().unwrap()
            );
            let restored = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let restored = api.get(&driver_path).await;
                    if restored["driver"]["state"] == "interrupted" {
                        break restored;
                    }
                    assert_ne!(restored["driver"]["state"], "failed", "{restored}");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("automatic startup must refuse the unknown accepted mapping");
            assert_eq!(restored["driver"]["active_session_id"], session);
            assert_eq!(restored["driver"]["launched_run_count"], 1);
            assert_eq!(restored["driver"]["pending_run_index"], 1);
            assert_eq!(
                api.get("/api/v1/launch/benchmark/suite/drivers").await["drivers"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            let retained = api
                .get(&format!(
                    "/api/v1/launch/benchmark/suites/{}",
                    suite["suite_id"].as_str().unwrap()
                ))
                .await;
            assert_eq!(retained["runs"][0]["state"], "interrupted");
            assert_eq!(retained["runs"][0]["session_id"], session);
            assert_eq!(retained["runs"][0]["launch_intent"], intent);
            assert_eq!(retained["runs"][1], suite["runs"][1]);
        } else {
            let replay = api
                .client
                .post(format!("{}/api/v1/launch", api.base))
                .header(transport::CAPABILITY_HEADER, &api.capability)
                .json(&json!({"instance_id":instance,"intent_key":intent}))
                .send()
                .await
                .unwrap();
            assert_eq!(replay.status(), reqwest::StatusCode::CONFLICT);
            assert_eq!(replay.json::<Value>().await.unwrap()["code"], "interrupted");
        }
        for (method, path, body) in [
            (
                reqwest::Method::PUT,
                format!("/api/v1/instances/{instance}"),
                json!({"name":"Must not change"}),
            ),
            (
                reqwest::Method::DELETE,
                format!("/api/v1/instances/{instance}?keep_files=true"),
                Value::Null,
            ),
        ] {
            let response = api
                .client
                .request(method, format!("{}{path}", api.base))
                .header(transport::CAPABILITY_HEADER, &api.capability)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({
                    "error":axial_app::instances::model::InstanceError::Busy.to_string()
                })
            );
        }
        assert_eq!(
            api.get("/api/v1/launch/sessions").await,
            json!({"sessions":[]})
        );
        assert_eq!(
            api.get("/api/v1/launch/reports").await,
            json!({"reports":[]})
        );
        assert_eq!(
            unobserved_intent_payload(reopened.instances.registry().storage(), &intent),
            payload
        );
        assert_eq!(
            reopened
                .instances
                .registry()
                .get_live(&instance.parse().unwrap())
                .unwrap(),
            original_instance
        );
        for (path, bytes) in &native_files {
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(std::fs::read_dir(&native_root).unwrap().count(), 1);
        reopened.server.shutdown().await.unwrap();
        assert!(reopened.server.is_shutdown_settled());
        ping_fixture_channels(&mut channels).await;
        drop(reopened);
        if let Some((_, root)) = &external {
            assert_eq!(
                std::fs::read(root.join("user-canary.bin")).unwrap(),
                EXTERNAL_CANARY
            );
        }
    }

    // Only the test closes its channels. This cannot mint an application proof.
    for channel in &mut channels {
        channel.write_all(b"x").await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        for channel in &mut channels {
            assert_eq!(channel.read(&mut [0]).await.unwrap(), 0);
        }
    })
    .await
    .expect("test-owned fixture cleanup must finish");
    let storage = axial_app::storage::MetadataStore::open(profile.join("metadata.sqlite")).unwrap();
    assert_eq!(unobserved_intent_payload(&storage, &intent), payload);
    for (path, bytes) in &native_files {
        assert_eq!(&std::fs::read(path).unwrap(), bytes);
    }
    if let Some((_, root)) = &external {
        assert_eq!(
            std::fs::read(root.join("user-canary.bin")).unwrap(),
            EXTERNAL_CANARY
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess helper that exits with its actual game tree still running"]
async fn interrupted_launch_crash_helper() {
    let profile =
        PathBuf::from(std::env::var_os(INTERRUPTED_CHILD_PROFILE).expect("isolated child profile"));
    assert_eq!(std::fs::canonicalize(&profile).unwrap(), profile);
    let instance = std::env::var(INTERRUPTED_CHILD_INSTANCE).unwrap();
    let intent = std::env::var(INTERRUPTED_CHILD_INTENT).unwrap();
    let services = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&services);
    wait_launchable(&api, &instance).await;
    let (session, intent) = if std::env::var(INTERRUPTED_CHILD_BENCHMARK).as_deref() == Ok("1") {
        let driver = api.post("/api/v1/launch/benchmark/suite/driver", json!({
            "instance_id":instance,"suite_id":"interrupted-driver-suite","suite_mode":"development","interval_ms":30000
        })).await;
        let run = wait_benchmark_run(&api, "interrupted-driver-suite", 0).await;
        let path = format!(
            "/api/v1/launch/benchmark/suite/drivers/{}",
            driver["driver"]["id"].as_str().unwrap()
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while api.get(&path).await["driver"]["state"] != "waiting" {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("accepted driver must checkpoint its first mapping");
        (
            run["session_id"].as_str().unwrap().to_owned(),
            run["launch_intent"].as_str().unwrap().to_owned(),
        )
    } else {
        let accepted = api
            .post(
                "/api/v1/launch",
                json!({"instance_id":instance,"intent_key":intent}),
            )
            .await;
        (accepted["session_id"].as_str().unwrap().to_owned(), intent)
    };
    let (_, processes) = observe_running_session(&api, &session).await;
    assert_eq!(processes.len(), 2);
    let status = api.get(&format!("/api/v1/launch/{session}/status")).await;
    assert_eq!(status["process_alive"], true);
    assert_eq!(status["phase"], "running");
    let payload = unobserved_intent_payload(services.instances.registry().storage(), &intent);
    assert_eq!(
        serde_json::from_slice::<Value>(&payload).unwrap()["session_id"],
        session
    );
    std::process::exit(INTERRUPTED_CHILD_EXIT);
}

async fn wait_benchmark_run(api: &Api, suite: &str, index: usize) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let suite = api
                .get(&format!("/api/v1/launch/benchmark/suites/{suite}"))
                .await;
            if suite["runs"][index]["state"] == "running" {
                break suite["runs"][index].clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("automatic driver must accept the expected real session")
}

async fn assert_fixture_processes_gone(processes: &BTreeSet<u64>) {
    let gone = tokio::time::timeout(Duration::from_secs(5), tokio::process::Command::new("python3")
        .args(["-c", "import os, sys\nfor pid in sys.argv[1:]:\n    try: os.kill(int(pid), 0)\n    except ProcessLookupError: continue\n    raise SystemExit('fixture process remains alive: ' + pid)\n"])
        .args(processes.iter().map(u64::to_string)).kill_on_drop(true).output()
    ).await.expect("bounded fixture process check").unwrap();
    assert!(
        gone.status.success(),
        "{}",
        String::from_utf8_lossy(&gone.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_benchmark_driver_stop_resume_preserves_live_run_and_continues_once() {
    use axial_app::storage::StorageError;
    use futures_util::FutureExt;

    const SUITE: &str = "live-stop-resume-suite";
    let mut temporary =
        Some(tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap());
    let profile = temporary.as_ref().unwrap().path().join("profile");
    let mut preserve = || {
        if let Some(temporary) = temporary.take() {
            eprintln!(
                "Retained benchmark fixture after failure: {}",
                temporary.keep().display()
            );
        }
    };
    let provider = Provider::start(false).await;
    let services =
        match start_profile_with_test_endpoints(profile.clone(), provider.endpoints()).await {
            Ok(services) => services,
            Err(error) => {
                preserve();
                provider.shutdown().await;
                panic!("benchmark fixture startup failed: {error}");
            }
        };
    let mut processes = BTreeSet::new();
    let journey = std::panic::AssertUnwindSafe(async {
        let api = Api::new(&services);
        api.post(
            "/api/v1/accounts/offline",
            json!({"username":PLAYER,"expected_selection_revision":0}),
        )
        .await;
        api.request(
            reqwest::Method::PUT,
            "/api/v1/config",
            Some(json!({
                "expected_revision":0,"performance_mode":"vanilla","java_path_override":""
            })),
        )
        .await;
        let install = api
            .post(
                "/api/v1/install/queue",
                json!({"kind":"vanilla","version_id":VERSION}),
            )
            .await;
        let terminal = install_terminal(&api, &install).await;
        assert_eq!(terminal["outcome"], "succeeded", "{terminal}");
        let created = api
            .post(
                "/api/v1/instances",
                json!({"name":"Live scheduler continuation","selection_id":format!("vanilla|{VERSION}")}),
            )
            .await;
        assert!(created["install_queue"].is_null(), "{created}");
        let instance = created["id"].as_str().unwrap();
        let started = api
            .post(
                "/api/v1/launch/benchmark/suite/driver",
                json!({"instance_id":instance,"suite_id":SUITE,"suite_mode":"development","interval_ms":5000}),
            )
            .await;
        let driver_id = started["driver"]["id"].as_str().unwrap();
        let driver_path = format!("/api/v1/launch/benchmark/suite/drivers/{driver_id}");
        let suite_path = format!("/api/v1/launch/benchmark/suites/{SUITE}");
        let first = wait_benchmark_run(&api, SUITE, 0).await;
        let first_session = first["session_id"].as_str().unwrap();
        let (_, first_processes) = observe_running_session(&api, first_session).await;
        processes.extend(first_processes.iter().copied());
        let live = api
            .get(&format!("/api/v1/launch/{first_session}/status"))
            .await;
        assert!(first_processes.contains(&live["pid"].as_u64().unwrap()));
        let original_suite = api.get(&suite_path).await;
        assert_eq!(original_suite["runs"].as_array().unwrap().len(), 2);
        let first_intent = first["launch_intent"].as_str().unwrap();
        let second_intent = original_suite["runs"][1]["launch_intent"]
            .as_str()
            .unwrap();
        assert_ne!(first_intent, second_intent);
        for (index, profile, run_type) in [
            (0, "vanilla_baseline", "coldish"),
            (1, "managed_default", "repeat"),
        ] {
            assert_eq!(original_suite["runs"][index]["profile"], profile);
            assert_eq!(original_suite["runs"][index]["run_type"], run_type);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = api.get(&driver_path).await;
                if status["driver"]["state"] == "waiting" {
                    break;
                }
                assert_eq!(status["driver"]["state"], "running", "{status}");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("first accepted run must reach its public driver checkpoint");
        let request = services.instances.registry().storage().read(|db| -> Result<Vec<u8>, StorageError> {
            Ok(db.query_row("SELECT request FROM benchmark_drivers WHERE driver_id=?1", [driver_id], |row| row.get(0))?)
        }).unwrap();

        // No task-owner idle wait or mutation retry separates Stop from Resume.
        for (action, state) in [("stop", "stopped"), ("resume", "running")] {
            let response = api
                .client
                .post(format!("{}{driver_path}/{action}", api.base))
                .header(transport::CAPABILITY_HEADER, &api.capability)
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            let checkpoint = api.get(&driver_path).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{action}: {body}; driver: {checkpoint}"
            );
            if action == "resume" {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let status = api.get(&driver_path).await;
                        if status["driver"]["state"] == "waiting" {
                            assert_eq!(status["driver"]["active_session_id"], first_session);
                            break;
                        }
                        assert_eq!(status["driver"]["state"], "running", "{status}");
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("resumed scheduler must observe the original still-running session");
            }
            let suite = api.get(&suite_path).await;
            let session = api
                .get(&format!("/api/v1/launch/{first_session}/status"))
                .await;
            assert_eq!(body["driver"]["id"], driver_id);
            assert_eq!(body["driver"]["suite_id"], SUITE);
            assert_eq!(body["driver"]["state"], state);
            assert_eq!(body["driver"]["interval_ms"], 5000);
            assert_eq!(body["driver"]["launched_run_count"], 1);
            assert_eq!(body["driver"]["pending_run_index"], 1);
            assert_eq!(body["driver"]["active_session_id"], first_session);
            assert_eq!(body["driver"]["last_session_id"], first_session);
            assert_eq!(body["view_model"]["can_stop"], action == "resume");
            assert_eq!(body["view_model"]["can_resume"], action == "stop");
            assert_eq!(suite["runs"], original_suite["runs"]);
            assert_eq!(suite["runs"][1]["state"], "pending");
            assert!(suite["runs"][1]["session_id"].is_null());
            assert_eq!(session["phase"], "running");
            assert_eq!(session["process_alive"], true);
            assert_eq!(session["pid"], live["pid"]);
            let sessions = api.get("/api/v1/launch/sessions").await;
            assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);
            assert_eq!(sessions["sessions"][0]["session_id"], first_session);
            let unreserved = api
                .client
                .get(format!("{}/api/v1/launch/intents/{second_intent}", api.base))
                .header(transport::CAPABILITY_HEADER, &api.capability)
                .send()
                .await
                .unwrap();
            assert_eq!(unreserved.status(), StatusCode::NOT_FOUND);
        }

        assert_eq!(
            observe_and_stop_session(&api, first_session).await,
            first_processes
        );
        services.instances.registry().storage().read(|db| -> Result<(), StorageError> {
            assert_eq!(db.query_row("SELECT count(*) FROM launch_intents WHERE intent_key=?1 AND state='accepted' AND terminal_ack=1 AND settlement IS NOT NULL", [first_intent], |row| row.get::<_, i64>(0))?, 1);
            Ok(())
        }).unwrap();
        let second = wait_benchmark_run(&api, SUITE, 1).await;
        let second_session = second["session_id"].as_str().unwrap();
        assert_ne!(second_session, first_session);
        assert_eq!(second["launch_intent"], second_intent);
        let (_, second_processes) = observe_running_session(&api, second_session).await;
        processes.extend(second_processes.iter().copied());
        assert_eq!(
            observe_and_stop_session(&api, second_session).await,
            second_processes
        );
        let complete_driver = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let status = api.get(&driver_path).await;
                if status["driver"]["state"] == "complete" {
                    break status;
                }
                assert!(matches!(status["driver"]["state"].as_str(), Some("running" | "waiting")), "{status}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("resumed driver must complete its original two-run plan");
        assert_eq!(complete_driver["driver"]["id"], driver_id);
        assert_eq!(complete_driver["driver"]["run_count"], 2);
        assert_eq!(complete_driver["driver"]["launched_run_count"], 2);
        assert!(complete_driver["driver"]["pending_run_index"].is_null());
        assert!(complete_driver["driver"]["active_session_id"].is_null());
        assert!(!complete_driver["view_model"]["can_resume"].as_bool().unwrap());
        let complete_suite = api.get(&suite_path).await;
        assert_eq!(complete_suite["runs"].as_array().unwrap().len(), 2);
        let mut reports = Vec::new();
        for (index, session, intent) in [
            (0, first_session, first_intent),
            (1, second_session, second_intent),
        ] {
            let run = &complete_suite["runs"][index];
            assert_eq!(run["session_id"], session);
            assert_eq!(run["launch_intent"], intent);
            assert_eq!(run["state"], "stopped");
            for field in ["benchmark_id", "profile", "run_type", "target_id"] {
                assert_eq!(run[field], original_suite["runs"][index][field]);
            }
            let accepted = api.get(&format!("/api/v1/launch/intents/{intent}")).await;
            assert_eq!(accepted["state"], "accepted");
            assert_eq!(accepted["session"]["session_id"], session);
            assert_eq!(accepted["session"]["phase"], "exited");
            assert_eq!(accepted["session"]["tree_settled"], true);
            assert_eq!(accepted["session"]["output_drained"], true);
            let report = api.get(&format!("/api/v1/launch/reports/{session}")).await;
            assert_eq!(report["instance_id"], instance);
            assert_eq!(report["scenario"]["benchmark_id"], run["benchmark_id"]);
            assert_eq!(report["scenario"]["benchmark_profile"], run["profile"]);
            assert_eq!(report["scenario"]["benchmark_run_type"], run["run_type"]);
            assert_eq!(report["scenario"]["benchmark_mode"], "development");
            assert_eq!(report["session_outcome"]["kind"], "stopped");
            reports.push((session.to_owned(), report));
        }
        (driver_path, suite_path, complete_driver, complete_suite, reports, request)
    })
    .catch_unwind()
    .await;
    let shutdown = std::panic::AssertUnwindSafe(services.server.shutdown())
        .catch_unwind()
        .await;
    let settled = services.server.is_shutdown_settled();
    drop(services);
    let provider_shutdown = std::panic::AssertUnwindSafe(provider.shutdown())
        .catch_unwind()
        .await;
    let absent = std::panic::AssertUnwindSafe(assert_fixture_processes_gone(&processes))
        .catch_unwind()
        .await;
    if !matches!(&shutdown, Ok(Ok(())))
        || !settled
        || provider_shutdown.is_err()
        || absent.is_err()
        || journey.is_err()
    {
        preserve();
    }
    shutdown
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        .unwrap();
    assert!(settled);
    provider_shutdown.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    absent.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    let (driver_path, suite_path, driver, suite, reports, request) =
        journey.unwrap_or_else(|panic| std::panic::resume_unwind(panic));

    let reopened = match start_in_profile(profile, None).await {
        Ok(services) => services,
        Err(error) => {
            preserve();
            panic!("benchmark fixture reopen failed: {error}");
        }
    };
    let persisted = std::panic::AssertUnwindSafe(async {
        let api = Api::new(&reopened);
        assert_eq!(api.get(&driver_path).await, driver);
        assert_eq!(api.get(&suite_path).await, suite);
        assert_eq!(api.get("/api/v1/launch/sessions").await, json!({"sessions":[]}));
        for (session, report) in reports {
            assert_eq!(api.get(&format!("/api/v1/launch/reports/{session}")).await, report);
        }
        reopened.instances.registry().storage().read(|db| -> Result<(), StorageError> {
            let saved: Vec<u8> = db.query_row("SELECT request FROM benchmark_drivers WHERE driver_id=?1", [driver["driver"]["id"].as_str().unwrap()], |row| row.get(0))?;
            assert!(saved == request, "resumption must preserve the captured driver request");
            assert_eq!(db.query_row("SELECT count(*) FROM benchmark_drivers", [], |row| row.get::<_, i64>(0))?, 1);
            assert_eq!(db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get::<_, i64>(0))?, 2);
            assert_eq!(db.query_row("SELECT count(*) FROM launch_intents WHERE state='accepted' AND terminal_ack=1 AND settlement IS NOT NULL", [], |row| row.get::<_, i64>(0))?, 2);
            assert_eq!(db.query_row("SELECT count(*) FROM launch_reports", [], |row| row.get::<_, i64>(0))?, 2);
            Ok(())
        }).unwrap();
    })
    .catch_unwind()
    .await;
    let shutdown = std::panic::AssertUnwindSafe(reopened.server.shutdown())
        .catch_unwind()
        .await;
    let settled = reopened.server.is_shutdown_settled();
    drop(reopened);
    let absent = std::panic::AssertUnwindSafe(assert_fixture_processes_gone(&processes))
        .catch_unwind()
        .await;
    if !matches!(&shutdown, Ok(Ok(()))) || !settled || absent.is_err() || persisted.is_err() {
        preserve();
    }
    shutdown
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        .unwrap();
    assert!(settled);
    absent.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    persisted.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_benchmark_driver_automatically_resumes_remaining_run_once() {
    use axial_app::storage::StorageError;
    use std::process::Stdio;

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":0,"performance_mode":"vanilla","java_path_override":""
        })),
    )
    .await;
    let install = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    let terminal = install_terminal(&api, &install).await;
    assert_eq!(terminal["outcome"], "succeeded", "{terminal}");
    let created = api
        .post(
            "/api/v1/instances",
            json!({
                "name":"Automatic driver restart","selection_id":format!("vanilla|{VERSION}")
            }),
        )
        .await;
    let instance = created["id"].as_str().unwrap().to_owned();
    assert!(created["install_queue"].is_null(), "{created}");
    services.server.shutdown().await.unwrap();
    drop(services);
    provider.shutdown().await;

    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(temporary.path().join("driver-child.log"))
        .unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "offline_journey_tests::benchmark_pending_boundary_crash_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(DRIVER_CHILD_PROFILE, &profile)
        .env(DRIVER_CHILD_INSTANCE, &instance)
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output.try_clone().unwrap()))
        .kill_on_drop(false)
        .spawn()
        .unwrap();
    let helper_pid = child.id();
    let exit = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    if !matches!(&exit, Ok(Ok(status)) if status.code() == Some(DRIVER_CHILD_EXIT)) {
        let tail = fixture_child_log_tail(&mut output);
        let preserved = temporary.keep();
        panic!(
            "driver boundary helper did not settle safely: {exit:?}; helper {helper_pid:?}; retained {}\n{tail}",
            preserved.display()
        );
    }
    let (driver_bytes, request, suite_bytes, first_intent_bytes, first_report_bytes) = {
        let storage =
            axial_app::storage::MetadataStore::open(profile.join("metadata.sqlite")).unwrap();
        storage.read(|db| -> Result<_, StorageError> {
            let (driver, request): (Vec<u8>, Vec<u8>) = db.query_row("SELECT payload,request FROM benchmark_drivers", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
            let suite: Vec<u8> = db.query_row("SELECT payload FROM benchmark_suites WHERE suite_id=?1", [RESTART_SUITE], |row| row.get(0))?;
            let intent: Vec<u8> = db.query_row("SELECT payload FROM launch_intents WHERE state='accepted' AND terminal_ack=1 AND settlement IS NOT NULL", [], |row| row.get(0))?;
            let report: Vec<u8> = db.query_row("SELECT payload FROM launch_reports", [], |row| row.get(0))?;
            assert_eq!(db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get::<_, i64>(0))?, 1);
            Ok((driver, request, suite, intent, report))
        }).unwrap()
    };
    let driver: Value = serde_json::from_slice(&driver_bytes).unwrap();
    let original_suite: Value = serde_json::from_slice(&suite_bytes).unwrap();
    assert_eq!(driver["state"], "waiting");
    assert_eq!(driver["launched_run_count"], 1);
    assert_eq!(driver["pending_run_index"], 1);
    assert_eq!(original_suite["runs"][1]["state"], "pending");
    assert!(original_suite["runs"][1]["session_id"].is_null());
    let driver_id = driver["id"].as_str().unwrap();
    let driver_path = format!("/api/v1/launch/benchmark/suite/drivers/{driver_id}");
    let first = original_suite["runs"][0]["session_id"].as_str().unwrap();
    let first_intent = original_suite["runs"][0]["launch_intent"].as_str().unwrap();

    let reopened = start_in_profile(profile.clone(), None).await.unwrap();
    let api = Api::new(&reopened);
    // Startup itself owns this continuation; no Resume or Tick request follows.
    let second = wait_benchmark_run(&api, RESTART_SUITE, 1).await;
    let second_session = second["session_id"].as_str().unwrap();
    assert_ne!(second_session, first);
    assert_ne!(second["launch_intent"], first_intent);
    let processes = observe_and_stop_session(&api, second_session).await;
    assert_fixture_processes_gone(&processes).await;
    let complete_driver = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let status = api.get(&driver_path).await;
            if status["driver"]["state"] == "complete" {
                break status;
            }
            assert!(
                matches!(
                    status["driver"]["state"].as_str(),
                    Some("running" | "waiting")
                ),
                "{status}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("automatic driver must complete after the remaining real run");
    assert_eq!(complete_driver["driver"]["launched_run_count"], 2);
    assert!(complete_driver["driver"]["pending_run_index"].is_null());
    let complete_suite = api
        .get(&format!("/api/v1/launch/benchmark/suites/{RESTART_SUITE}"))
        .await;
    assert_eq!(complete_suite["runs"][0]["session_id"], first);
    assert_eq!(complete_suite["runs"][0]["launch_intent"], first_intent);
    assert_eq!(complete_suite["runs"][1]["session_id"], second_session);
    assert!(
        complete_suite["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|run| run["state"] == "stopped")
    );
    for (index, session) in [first, second_session].into_iter().enumerate() {
        let report = api.get(&format!("/api/v1/launch/reports/{session}")).await;
        assert_eq!(
            report["scenario"]["benchmark_id"],
            complete_suite["runs"][index]["benchmark_id"]
        );
        assert_eq!(report["session_outcome"]["kind"], "stopped");
    }
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
    drop(reopened);

    let reopened = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&reopened);
    assert_eq!(api.get(&driver_path).await, complete_driver);
    assert_eq!(
        api.get(&format!("/api/v1/launch/benchmark/suites/{RESTART_SUITE}"))
            .await,
        complete_suite
    );
    assert_eq!(
        api.get("/api/v1/launch/sessions").await,
        json!({"sessions":[]})
    );
    reopened.instances.registry().storage().read(|db| -> Result<(), StorageError> {
        assert_eq!(db.query_row("SELECT count(*) FROM benchmark_drivers", [], |row| row.get::<_, i64>(0))?, 1);
        assert_eq!(db.query_row("SELECT request FROM benchmark_drivers WHERE driver_id=?1", [driver_id], |row| row.get::<_, Vec<u8>>(0))?, request);
        assert_eq!(db.query_row("SELECT count(*) FROM launch_intents WHERE state='accepted' AND terminal_ack=1 AND settlement IS NOT NULL", [], |row| row.get::<_, i64>(0))?, 2);
        assert_eq!(db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get::<_, i64>(0))?, 2);
        assert_eq!(db.query_row("SELECT count(*) FROM launch_reports", [], |row| row.get::<_, i64>(0))?, 2);
        assert_eq!(db.query_row("SELECT payload FROM launch_intents WHERE intent_key=?1", [first_intent], |row| row.get::<_, Vec<u8>>(0))?, first_intent_bytes);
        assert_eq!(db.query_row("SELECT payload FROM launch_reports WHERE session_id=?1", [first], |row| row.get::<_, Vec<u8>>(0))?, first_report_bytes);
        Ok(())
    }).unwrap();
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess helper that exits only between fully settled benchmark runs"]
async fn benchmark_pending_boundary_crash_helper() {
    use futures_util::FutureExt;
    use tracing_subscriber::prelude::*;

    let profile =
        PathBuf::from(std::env::var_os(DRIVER_CHILD_PROFILE).expect("isolated driver profile"));
    assert_eq!(std::fs::canonicalize(&profile).unwrap(), profile);
    let instance = std::env::var(DRIVER_CHILD_INSTANCE).unwrap();
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_ansi(false)
        .without_time()
        .finish()
        .with(
            tracing_subscriber::filter::Targets::new()
                .with_target("axial_app::launch::session", tracing::Level::WARN)
                .with_target("axial_app::launch::prepare", tracing::Level::WARN),
        )
        .try_init()
        .unwrap();
    let services = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&services);
    let boundary = std::panic::AssertUnwindSafe(async {
        let started = std::time::Instant::now();
        let driver = api.post("/api/v1/launch/benchmark/suite/driver", json!({
            "instance_id":instance,"suite_id":RESTART_SUITE,"suite_mode":"development","interval_ms":30000
        })).await;
        let run = wait_benchmark_run(&api, RESTART_SUITE, 0).await;
        let session = run["session_id"].as_str().unwrap();
        let processes = observe_and_stop_session(&api, session).await;
        assert_fixture_processes_gone(&processes).await;
        let native_root = services.library.admit().unwrap().read_projection().unwrap().join("cache/natives");
        assert_eq!(std::fs::read_dir(&native_root).unwrap().count(), 0);
        assert!(services.sessions.snapshots().iter().all(|session| session.phase == axial_app::launch::session::SessionPhase::Exited));
        let report = api.get(&format!("/api/v1/launch/reports/{session}")).await;
        assert_eq!(report["session_outcome"]["kind"], "stopped");
        services.instances.registry().storage().read(|db| -> Result<(), axial_app::storage::StorageError> {
            assert_eq!(db.query_row("SELECT count(*) FROM launch_intents WHERE state='accepted' AND terminal_ack=1 AND settlement IS NOT NULL", [], |row| row.get::<_, i64>(0))?, 1);
            assert_eq!(db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get::<_, i64>(0))?, 1);
            Ok(())
        }).unwrap();
        let path = format!("/api/v1/launch/benchmark/suite/drivers/{}", driver["driver"]["id"].as_str().unwrap());
        let current = api.get(&path).await;
        assert_eq!(current["driver"]["state"], "waiting");
        assert_eq!(current["driver"]["pending_run_index"], 1);
        let suite = api.get(&format!("/api/v1/launch/benchmark/suites/{RESTART_SUITE}")).await;
        assert_eq!(suite["runs"][0]["session_id"], session);
        assert_eq!(suite["runs"][1]["state"], "pending");
        assert!(suite["runs"][1]["session_id"].is_null());
        started.elapsed() < Duration::from_secs(15)
    }).catch_unwind().await;
    if matches!(&boundary, Ok(true)) {
        std::process::exit(DRIVER_CHILD_EXIT);
    }
    // A missed safe boundary is a failed test, never permission to kill a game.
    if let Err(error) = services.server.shutdown().await {
        eprintln!("driver fixture cleanup requires inspection: {error}");
        std::future::pending::<()>().await;
    }
    match boundary {
        Err(panic) => std::panic::resume_unwind(panic),
        _ => panic!("settled pending boundary exceeded its safe pre-tick deadline"),
    }
}

fn settlement_evidence(
    storage: &axial_app::storage::MetadataStore,
    intent: &str,
) -> (Vec<u8>, Vec<u8>) {
    use axial_app::storage::StorageError;
    storage
        .read(|db| -> Result<_, StorageError> {
            let (payload, settlement, acknowledged): (Vec<u8>, Option<Vec<u8>>, i64) = db
                .query_row(
                    "SELECT payload,settlement,terminal_ack FROM launch_intents WHERE intent_key=?1 AND state='accepted'",
                    [intent],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            let settlement = settlement.expect("the real session committed settlement before report failure");
            assert!(!settlement.is_empty() && settlement.len() <= 4096);
            assert_eq!(acknowledged, 0, "no report was acknowledged");
            assert_eq!(db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get::<_, i64>(0))?, 1);
            assert_eq!(db.query_row("SELECT count(*) FROM launch_reports", [], |row| row.get::<_, i64>(0))?, 0);
            assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='refuse_settled_report'", [], |row| row.get::<_, i64>(0))?, 1);
            Ok((payload, settlement))
        })
        .unwrap()
}

fn assert_observed_terminal(status: &Value, instance: &str, session: &str) {
    assert_eq!(status["state"], "accepted", "{status}");
    let snapshot = &status["session"];
    assert_eq!(snapshot["session_id"], session);
    assert_eq!(snapshot["instance_id"], instance);
    assert_eq!(snapshot["phase"], "exited");
    assert_eq!(snapshot["tree_settled"], true);
    assert_eq!(snapshot["output_drained"], true);
    assert_eq!(snapshot["process_alive"], false);
    assert_eq!(snapshot["stop_allowed"], false);
    assert_eq!(snapshot["boot_observed"], true);
    assert_eq!(snapshot["view_model"]["terminal"], true);
    assert_eq!(snapshot["outcome"]["kind"], "stopped");
    assert_eq!(snapshot["outcome"]["reason"], "launcher_stopped");
    assert_eq!(snapshot["notice"]["tone"], "warned");
    assert!(
        snapshot["notice"]["message"]
            .as_str()
            .unwrap()
            .contains("report is unavailable")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_observed_settlement_survives_report_failure_and_process_restart() {
    use std::process::Stdio;

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":0,"performance_mode":"vanilla","java_path_override":""
        })),
    )
    .await;
    let install = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    let terminal = install_terminal(&api, &install).await;
    assert_eq!(terminal["outcome"], "succeeded", "{terminal}");
    let created = api
        .post(
            "/api/v1/instances",
            json!({
                "name":"Observed settlement","selection_id":format!("vanilla|{VERSION}")
            }),
        )
        .await;
    let instance = created["id"].as_str().unwrap().to_owned();
    assert!(created["install_queue"].is_null(), "{created}");
    let native_root = services
        .library
        .admit()
        .unwrap()
        .read_projection()
        .unwrap()
        .join("cache/natives");
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    // The fresh launcher uses only the real downloaded installation. Its exit
    // bypasses service shutdown, but only after the owned game tree is gone.
    let intent = uuid::Uuid::new_v4().to_string();
    let mut output = tempfile::tempfile_in(temporary.path()).unwrap();
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "offline_journey_tests::observed_settlement_crash_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(SETTLED_CHILD_PROFILE, &profile)
        .env(SETTLED_CHILD_INSTANCE, &instance)
        .env(SETTLED_CHILD_INTENT, &intent)
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output.try_clone().unwrap()))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert_fixture_child_exit(child, &mut output, SETTLED_CHILD_EXIT).await;

    let evidence = {
        let storage =
            axial_app::storage::MetadataStore::open(profile.join("metadata.sqlite")).unwrap();
        settlement_evidence(&storage, &intent)
    };
    let accepted: Value = serde_json::from_slice(&evidence.0).unwrap();
    let session = accepted["session_id"].as_str().unwrap();
    assert_eq!(accepted["request"]["instance_id"], instance);
    assert_eq!(accepted["request"]["intent_key"], intent);
    let reopened = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&reopened);
    let recovered = api.get(&format!("/api/v1/launch/intents/{intent}")).await;
    assert_observed_terminal(&recovered, &instance, session);
    let replay = api
        .post(
            "/api/v1/launch",
            json!({"instance_id":instance,"intent_key":intent}),
        )
        .await;
    assert_eq!(replay, recovered["session"]);
    assert_eq!(
        api.get("/api/v1/launch/sessions").await,
        json!({"sessions":[]})
    );
    assert_eq!(
        api.get("/api/v1/launch/reports").await,
        json!({"reports":[]})
    );
    assert_eq!(std::fs::read_dir(&native_root).unwrap().count(), 0);
    assert_eq!(
        settlement_evidence(reopened.instances.registry().storage(), &intent),
        evidence
    );
    wait_launchable(&api, &instance).await;
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess helper that exits after real settlement while report writes remain refused"]
async fn observed_settlement_crash_helper() {
    use axial_app::storage::StorageError;

    let profile =
        PathBuf::from(std::env::var_os(SETTLED_CHILD_PROFILE).expect("isolated child profile"));
    assert_eq!(std::fs::canonicalize(&profile).unwrap(), profile);
    let instance = std::env::var(SETTLED_CHILD_INSTANCE).unwrap();
    let intent = std::env::var(SETTLED_CHILD_INTENT).unwrap();
    let services = start_in_profile(profile, None).await.unwrap();
    services.instances.registry().storage().transaction(|tx| -> Result<(), StorageError> {
        tx.execute_batch("CREATE TRIGGER refuse_settled_report BEFORE INSERT ON launch_reports BEGIN SELECT RAISE(FAIL,'fixture report unavailable'); END;")?;
        Ok(())
    }).unwrap();
    let api = Api::new(&services);
    wait_launchable(&api, &instance).await;
    let accepted = api
        .post(
            "/api/v1/launch",
            json!({"instance_id":instance,"intent_key":intent}),
        )
        .await;
    let session = accepted["session_id"].as_str().unwrap();
    let native_root = services
        .library
        .admit()
        .unwrap()
        .read_projection()
        .unwrap()
        .join("cache/natives");
    let natives = std::fs::read_dir(&native_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(natives.len(), 1);
    assert_eq!(
        std::fs::read(natives[0].join(native_name())).unwrap(),
        NATIVE
    );
    assert!(natives[0].join(".axial-native-manifest.json").is_file());
    let processes = observe_and_stop_session(&api, session).await;
    assert!(
        !natives[0].exists(),
        "owned native directory must be removed before helper exit"
    );
    let gone = tokio::time::timeout(Duration::from_secs(5), tokio::process::Command::new("python3")
        .args(["-c", "import os, sys\nfor pid in sys.argv[1:]:\n    try: os.kill(int(pid), 0)\n    except ProcessLookupError: continue\n    raise SystemExit('fixture process remains alive: ' + pid)\n"])
        .args(processes.iter().map(u64::to_string))
        .kill_on_drop(true)
        .output()).await.expect("bounded fixture process check").unwrap();
    assert!(
        gone.status.success(),
        "{}",
        String::from_utf8_lossy(&gone.stderr)
    );
    let status = api.get(&format!("/api/v1/launch/intents/{intent}")).await;
    assert_observed_terminal(&status, &instance, session);
    let evidence = settlement_evidence(services.instances.registry().storage(), &intent);
    let stored: Value = serde_json::from_slice(&evidence.0).unwrap();
    assert_eq!(stored["session_id"], session);
    assert_eq!(
        api.get("/api/v1/launch/reports").await,
        json!({"reports":[]})
    );
    std::process::exit(SETTLED_CHILD_EXIT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_preserves_safe_memory_override_and_budget_diagnostics() {
    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    let config = api
        .request(
            reqwest::Method::PUT,
            "/api/v1/config",
            Some(json!({
                "expected_revision":0,"performance_mode":"vanilla","jvm_preset":"",
                "java_path_override":"","max_memory_mb":2048,"min_memory_mb":1024
            })),
        )
        .await;
    let start = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    assert_eq!(install_terminal(&api, &start).await["outcome"], "succeeded");
    let created = api
        .post(
            "/api/v1/instances",
            json!({"name":"Preflight diagnostics","selection_id":format!("vanilla|{VERSION}")}),
        )
        .await;
    let instance = created["id"].as_str().unwrap();
    let preflight = format!("/api/v1/launch/preflight/{instance}");
    let reports_before = api.get("/api/v1/launch/reports").await;
    let queue_before = api.get("/api/v1/install/queue").await;
    let inherited = api.get(&preflight).await;
    let java = services
        .installs
        .runtime_cache()
        .root()
        .join(COMPONENT)
        .join(java_relative_path());
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":config["revision"],"java_path_override":java,
            "jvm_preset":"performance"
        })),
    )
    .await;
    let global = api.get(&preflight).await;
    let current = api.get(&format!("/api/v1/instances/{instance}")).await;
    let private_args = "-Dpreflight.private=preflight-secret-sentinel";
    api.request(
        reqwest::Method::PUT,
        &format!("/api/v1/instances/{instance}"),
        Some(json!({
            "expected_revision":current["revision"],"java_path":java,"jvm_preset":"performance",
            "extra_jvm_args":private_args,"max_memory_mb":768,"min_memory_mb":1536
        })),
    )
    .await;
    let local = api.get(&preflight).await;
    let sessions = api.get("/api/v1/launch/sessions").await;
    let reports_after = api.get("/api/v1/launch/reports").await;
    let queue_after = api.get("/api/v1/install/queue").await;
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    assert_eq!(sessions, json!({"sessions":[]}));
    assert_eq!(reports_before, json!({"reports":[]}));
    assert_eq!(reports_after, reports_before);
    assert_eq!(queue_after, queue_before);
    for (response, maximum, minimum, clamped, overrides) in [
        (
            inherited,
            2048,
            1024,
            false,
            json!({
                "java":{"present":false},"preset":{"present":false},"raw_jvm_args":{"present":false}
            }),
        ),
        (
            global,
            2048,
            1024,
            false,
            json!({
                "java":{"present":true,"origin":"global"},"preset":{"present":true,"origin":"global"},
                "raw_jvm_args":{"present":false}
            }),
        ),
        (
            local,
            768,
            768,
            true,
            json!({
                "java":{"present":true,"origin":"instance"},"preset":{"present":true,"origin":"instance"},
                "raw_jvm_args":{"present":true,"origin":"instance"}
            }),
        ),
    ] {
        assert_eq!(response["status"], "ready", "{response}");
        assert_eq!(response["instance_id"], instance);
        assert_eq!(response["launchable"], true, "{response}");
        assert_eq!(response.get("error"), Some(&Value::Null));
        assert_eq!(
            response["readiness"],
            json!({"launchable":true,"reasons":[]})
        );
        assert_eq!(
            response["memory"],
            json!({
                "max_memory_mb":maximum,"min_memory_mb":minimum,"min_clamped":clamped
            })
        );
        assert_eq!(response["overrides"], overrides);
        assert_eq!(response.as_object().unwrap().len(), 8);
        let budget = response["resource_budget"]
            .as_object()
            .expect("safe resource budget");
        assert_eq!(budget.len(), 9);
        assert_eq!(budget["active_session_count"], 0);
        assert_eq!(budget["active_install_count"], 0);
        assert_eq!(budget["active_memory_allocation_mb"], 0);
        assert_eq!(budget["requested_memory_mb"], maximum);
        let remaining = budget.get("estimated_remaining_memory_mb").unwrap();
        assert!(remaining.is_null() || remaining.as_i64().is_some());
        for pressure in [
            "memory_pressure",
            "cpu_pressure",
            "install_pressure",
            "disk_pressure",
        ] {
            assert!(budget[pressure].is_boolean(), "{pressure}: {budget:?}");
        }
        assert_eq!(budget["install_pressure"], false);
        let encoded = response.to_string();
        for private in [
            profile.to_str().unwrap(),
            java.to_str().unwrap(),
            private_args,
            "preflight-secret-sentinel",
            &api.capability,
        ] {
            assert!(
                !encoded.contains(private),
                "preflight exposed private input"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_reports_publication_contention_without_waiting_or_launching() {
    use futures_util::FutureExt;

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile, provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    let journey = std::panic::AssertUnwindSafe(async {
        api.post(
            "/api/v1/accounts/offline",
            json!({"username":PLAYER,"expected_selection_revision":0}),
        )
        .await;
        api.request(
            reqwest::Method::PUT,
            "/api/v1/config",
            Some(json!({"expected_revision":0,"performance_mode":"vanilla"})),
        )
        .await;
        let install = api
            .post(
                "/api/v1/install/queue",
                json!({"kind":"vanilla","version_id":VERSION}),
            )
            .await;
        assert_eq!(
            install_terminal(&api, &install).await["outcome"],
            "succeeded"
        );
        let created = api
            .post(
                "/api/v1/instances",
                json!({"name":"Publication preflight","selection_id":format!("vanilla|{VERSION}")}),
            )
            .await;
        let preflight = format!(
            "/api/v1/launch/preflight/{}",
            created["id"].as_str().unwrap()
        );
        assert_eq!(api.get(&preflight).await["launchable"], true);
        let pin = services.library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        let root = pin.read_projection().unwrap();
        let protected: Vec<_> = ["json", "jar"]
            .into_iter()
            .map(|extension| {
                let path = root.join(format!("versions/{VERSION}/{VERSION}.{extension}"));
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        let queue_before = api.get("/api/v1/install/queue").await;
        let requests_before = provider.state.requests.lock().unwrap().clone();
        let publication =
            axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).unwrap();
        let started = std::time::Instant::now();
        let busy = api.get(&preflight).await;
        let elapsed = started.elapsed();
        drop(publication);
        let recovered = api.get(&preflight).await;
        let target_busy_guard = services
            .instances
            .directories()
            .exclusions()
            .try_acquire([created["id"].as_str().unwrap()], [])
            .unwrap();
        let target_busy = api.get(&preflight).await;
        drop(target_busy_guard);
        let lane = root.join(".axial-publication");
        let retained_lane = temporary.path().join("retained-publication");
        std::fs::rename(&lane, &retained_lane).unwrap();
        let conflict = b"non-directory publication lane";
        std::fs::write(&lane, conflict).unwrap();
        let unsafe_lane = api.get(&preflight).await;
        let conflict_after = std::fs::read(&lane).unwrap();
        std::fs::remove_file(&lane).unwrap();
        std::fs::rename(&retained_lane, &lane).unwrap();
        let restored = api.get(&preflight).await;
        let requests_after = provider.state.requests.lock().unwrap().clone();
        let queue_after = api.get("/api/v1/install/queue").await;
        let sessions = api.get("/api/v1/launch/sessions").await;
        let reports = api.get("/api/v1/launch/reports").await;
        let protected_after: Vec<_> = protected
            .iter()
            .map(|(path, _)| std::fs::read(path).unwrap())
            .collect();
        move || {
            assert_eq!(requests_after, requests_before);
            assert_eq!(queue_after, queue_before);
            assert_eq!(sessions, json!({"sessions":[]}));
            assert_eq!(reports, json!({"reports":[]}));
            for ((_, before), after) in protected.into_iter().zip(protected_after) {
                assert_eq!(after, before);
            }
            assert_eq!(conflict_after, conflict);
            for ready in [recovered, restored] {
                assert_eq!(ready["launchable"], true, "{ready}");
                assert_eq!(ready["readiness"], json!({"launchable":true,"reasons":[]}));
            }
            assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
            assert_eq!(busy["launchable"], false, "{busy}");
            assert_eq!(busy["error"]["code"], "instance_busy", "{busy}");
            assert_eq!(busy["status"], "ready", "{busy}");
            assert_eq!(
                busy["readiness"],
                json!({"launchable":false,"reasons":[{
                    "id":"incomplete_install","severity":"blocking",
                    "message":"Installation is changing. Wait for it to finish before launching."
                }]}),
                "{busy}"
            );
            assert_eq!(
                target_busy["error"]["code"], "instance_busy",
                "{target_busy}"
            );
            assert_eq!(target_busy["readiness"], Value::Null, "{target_busy}");
            assert_eq!(target_busy["launchable"], false, "{target_busy}");
            assert_eq!(
                unsafe_lane["error"]["code"], "library_unavailable",
                "{unsafe_lane}"
            );
            assert_eq!(unsafe_lane["status"], "ready", "{unsafe_lane}");
            assert_eq!(unsafe_lane["launchable"], false, "{unsafe_lane}");
            assert_eq!(
                unsafe_lane["readiness"],
                json!({"launchable":false,"reasons":[{
                    "id":"installed_versions_degraded","severity":"blocking",
                    "message":"Installed versions could not be inspected safely."
                }]}),
                "{unsafe_lane}"
            );
        }
    })
    .catch_unwind()
    .await;
    let shutdown = std::panic::AssertUnwindSafe(services.server.shutdown())
        .catch_unwind()
        .await;
    let settled = services.server.is_shutdown_settled();
    drop(services);
    let provider_join = std::panic::AssertUnwindSafe(provider.shutdown())
        .catch_unwind()
        .await;
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let shutdown = match shutdown {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        assert!(shutdown.is_ok(), "{shutdown:?}");
        assert!(settled);
        if let Err(panic) = provider_join {
            std::panic::resume_unwind(panic);
        }
        match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }));
    if let Err(panic) = verification {
        eprintln!(
            "Retained publication preflight fixture: {}",
            temporary.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_reports_unrelated_degraded_versions_without_repairing() {
    use futures_util::FutureExt;

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile, provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    let journey = std::panic::AssertUnwindSafe(async {
        api.post(
            "/api/v1/accounts/offline",
            json!({"username":PLAYER,"expected_selection_revision":0}),
        )
        .await;
        api.request(
            reqwest::Method::PUT,
            "/api/v1/config",
            Some(json!({"expected_revision":0,"performance_mode":"vanilla"})),
        )
        .await;
        let install = api
            .post(
                "/api/v1/install/queue",
                json!({"kind":"vanilla","version_id":VERSION}),
            )
            .await;
        assert_eq!(
            install_terminal(&api, &install).await["outcome"],
            "succeeded"
        );
        let created = api
            .post(
                "/api/v1/instances",
                json!({"name":"Degraded scan preflight","selection_id":format!("vanilla|{VERSION}")}),
            )
            .await;
        let instance = created["id"].as_str().unwrap().to_owned();
        let detail_path = format!("/api/v1/instances/{instance}");
        let preflight_path = format!("/api/v1/launch/preflight/{instance}");
        assert_eq!(api.get(&preflight_path).await["launchable"], true);
        assert_eq!(
            api.get(&detail_path).await["launch_action"]["primary_action"],
            "launch"
        );
        assert_installed(&api, true).await;
        let root = services.library.admit().unwrap().read_projection().unwrap();
        let client = root.join(format!("versions/{VERSION}/{VERSION}.jar"));
        let original_client = std::fs::read(&client).unwrap();
        assert_eq!(
            original_client,
            provider.state.routes["GET /artifacts/client.jar"]
        );
        let classifier = if cfg!(target_os = "macos") {
            "natives-macos"
        } else {
            "natives-linux"
        };
        let asset_hash = sha1(ASSET);
        let protected: Vec<_> = [
            format!("versions/{VERSION}/{VERSION}.json"),
            format!("versions/{VERSION}/{VERSION}.jar"),
            "libraries/org/axial/fixture/1.0/fixture-1.0.jar".to_owned(),
            format!("libraries/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-{classifier}.jar"),
            "assets/log_configs/fixture-log.xml".to_owned(),
            "assets/indexes/fixture-assets.json".to_owned(),
            format!("assets/objects/{}/{asset_hash}", &asset_hash[..2]),
        ]
        .into_iter()
        .map(|relative| {
            let path = root.join(relative);
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
        let queue_before = api.get("/api/v1/install/queue").await;
        let requests_before = provider.state.requests.lock().unwrap().clone();
        let read_projection = |path: &str| {
            let request = api.client.get(format!("{}{path}", api.base))
                .header(transport::CAPABILITY_HEADER, &api.capability);
            async move {
                let response = request.send().await.unwrap();
                (response.status(), response.json::<Value>().await.unwrap())
            }
        };
        // This is an external library entry, not another accepted installation.
        let unrelated = root.join("versions/external-degraded-entry");
        assert!(std::fs::symlink_metadata(&unrelated)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound));
        std::fs::create_dir(&unrelated).unwrap();
        let metadata = unrelated.join("external-degraded-entry.json");
        let malformed = b"{not valid version metadata\n";
        std::fs::write(&metadata, malformed).unwrap();
        let degraded_versions = read_projection("/api/v1/versions").await;
        let degraded = read_projection(&preflight_path).await;
        let degraded_detail = read_projection(&detail_path).await;
        let degraded_list = read_projection("/api/v1/instances").await;
        let intact: Vec<_> = protected.iter()
            .map(|(path, _)| std::fs::read(path).unwrap()).collect();
        let malformed_after = std::fs::read(&metadata).unwrap();

        std::fs::remove_file(&client).unwrap();
        let missing_before = std::fs::symlink_metadata(&client)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let combined_versions = read_projection("/api/v1/versions").await;
        let combined = read_projection(&preflight_path).await;
        let combined_detail = read_projection(&detail_path).await;
        let combined_list = read_projection("/api/v1/instances").await;
        let missing_after = std::fs::symlink_metadata(&client)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let malformed_combined = std::fs::read(&metadata).unwrap();
        std::fs::write(&client, &original_client).unwrap();
        std::fs::remove_file(&metadata).unwrap();
        std::fs::remove_dir(&unrelated).unwrap();
        let unrelated_absent = std::fs::symlink_metadata(&unrelated)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let restored_versions = read_projection("/api/v1/versions").await;
        let restored = read_projection(&preflight_path).await;
        let restored_detail = read_projection(&detail_path).await;
        let restored_list = read_projection("/api/v1/instances").await;
        let restored_bytes: Vec<_> = protected.iter()
            .map(|(path, _)| std::fs::read(path).unwrap()).collect();
        let requests_after = provider.state.requests.lock().unwrap().clone();
        let queue_after = api.get("/api/v1/install/queue").await;
        let sessions = api.get("/api/v1/launch/sessions").await;
        let reports = api.get("/api/v1/launch/reports").await;
        move || {
            assert_eq!(requests_after, requests_before);
            assert_eq!(queue_after, queue_before);
            assert_eq!(sessions, json!({"sessions":[]}));
            assert_eq!(reports, json!({"reports":[]}));
            for (((_, before), intact), restored) in
                protected.into_iter().zip(intact).zip(restored_bytes)
            {
                assert_eq!(intact, before);
                assert_eq!(restored, before);
            }
            assert_eq!(malformed_after, malformed);
            assert_eq!(malformed_combined, malformed);
            assert!(missing_before && missing_after, "preflight must not repair the client");
            assert!(unrelated_absent);
            for (status, versions) in [&degraded_versions, &combined_versions] {
                assert_eq!(*status, StatusCode::OK, "{versions}");
                assert_eq!(versions["scan_state"]["degraded"], true, "{versions}");
            }
            let installed = degraded_versions.1["versions"].as_array().unwrap().iter()
                .find(|version| version["id"] == VERSION).unwrap();
            assert_eq!(installed["installed"], true);
            assert_eq!(installed["launchable"], true, "{installed}");
            for (status, response) in [&restored_versions, &restored, &restored_detail, &restored_list] {
                assert_eq!(*status, StatusCode::OK, "{response}");
            }
            assert_eq!(restored_versions.1["scan_state"]["degraded"], false);
            assert_eq!(restored.1["launchable"], true, "{restored:?}");
            assert_eq!(restored.1["readiness"], json!({"launchable":true,"reasons":[]}));
            assert_eq!(restored_list.1["instances"].as_array().unwrap().len(), 1);
            for detail in [&restored_detail.1, &restored_list.1["instances"][0]] {
                assert_eq!(detail["id"], instance);
                assert_eq!(detail["launch_action"]["launchable"], true);
                assert_eq!(detail["launch_action"]["primary_action"], "launch");
            }
            let degraded_reason = json!({
                "id":"installed_versions_degraded","severity":"blocking",
                "message":"Could not verify installed versions. Check the library folder and try again."
            });
            for ((status, response), expected) in [
                (degraded, json!([degraded_reason.clone()])),
                (combined, json!([
                    {"id":"client_jar_missing","severity":"blocking",
                     "message":"Client game files are missing. Install this version before launching."},
                    degraded_reason
                ])),
            ] {
                assert_eq!(status, StatusCode::OK, "{response}");
                assert_eq!(response["instance_id"], instance);
                assert_eq!(response["status"], "ready", "{response}");
                assert_eq!(response["launchable"], false, "{response}");
                assert_eq!(response["error"]["code"], "installed_versions_degraded");
                assert_eq!(response["error"]["error"],
                    "Could not verify installed versions. Check the library folder and try again.");
                let mut reasons = response["readiness"]["reasons"].clone();
                reasons.as_array_mut().unwrap().sort_by(|left, right| {
                    left["id"].as_str().cmp(&right["id"].as_str())
                });
                assert_eq!(response["readiness"]["launchable"], false);
                assert_eq!(reasons, expected);
            }
            for ((detail_status, detail), (list_status, list)) in
                [(degraded_detail, degraded_list), (combined_detail, combined_list)]
            {
                assert_eq!(detail_status, StatusCode::OK, "{detail}");
                assert_eq!(list_status, StatusCode::OK, "{list}");
                assert_eq!(list["instances"].as_array().unwrap().len(), 1);
                for observed in [&detail, &list["instances"][0]] {
                    assert_eq!(observed["id"], instance);
                    assert_eq!(observed["launch_action"]["launchable"], false);
                    assert_eq!(observed["launch_action"]["primary_action"], "blocked");
                    assert_eq!(observed["needs_install"], "");
                }
            }
        }
    })
    .catch_unwind()
    .await;
    let shutdown = std::panic::AssertUnwindSafe(services.server.shutdown())
        .catch_unwind()
        .await;
    let settled = services.server.is_shutdown_settled();
    drop(services);
    let provider_join = std::panic::AssertUnwindSafe(provider.shutdown())
        .catch_unwind()
        .await;
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match shutdown {
            Ok(result) => assert!(result.is_ok(), "{result:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
        assert!(settled);
        if let Err(panic) = provider_join {
            std::panic::resume_unwind(panic);
        }
        match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }));
    if let Err(panic) = verification {
        eprintln!(
            "Retained degraded-scan preflight fixture: {}",
            temporary.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_reports_installed_file_damage_without_launching_or_repairing() {
    use std::os::unix::fs::MetadataExt;

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":0,"performance_mode":"vanilla","jvm_preset":"",
            "java_path_override":"","max_memory_mb":2048,"min_memory_mb":1024
        })),
    )
    .await;
    let start = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    assert_eq!(install_terminal(&api, &start).await["outcome"], "succeeded");
    let created = api
        .post(
            "/api/v1/instances",
            json!({"name":"Installed file preflight","selection_id":format!("vanilla|{VERSION}")}),
        )
        .await;
    let instance = created["id"].as_str().unwrap();
    let current = api.get(&format!("/api/v1/instances/{instance}")).await;
    let private_args = "-Dpreflight.private=missing-client-secret";
    api.request(
        reqwest::Method::PUT,
        &format!("/api/v1/instances/{instance}"),
        Some(json!({"expected_revision":current["revision"],"extra_jvm_args":private_args})),
    )
    .await;
    let preflight = format!("/api/v1/launch/preflight/{instance}");
    let ready = api.get(&preflight).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(ready["launchable"], true, "{ready}");
    assert_eq!(ready["readiness"], json!({"launchable":true,"reasons":[]}));
    let reports_before = api.get("/api/v1/launch/reports").await;
    let queue_before = api.get("/api/v1/install/queue").await;
    let sessions_before = api.get("/api/v1/launch/sessions").await;
    let library = services.library.admit().unwrap().read_projection().unwrap();
    let client = library.join(format!("versions/{VERSION}/{VERSION}.jar"));
    let original = std::fs::read(&client).unwrap();
    assert_eq!(original, provider.state.routes["GET /artifacts/client.jar"]);
    let required_library = library.join("libraries/org/axial/fixture/1.0/fixture-1.0.jar");
    let original_library = std::fs::read(&required_library).unwrap();
    assert_eq!(
        original_library,
        provider.state.routes["GET /artifacts/library.jar"]
    );
    std::fs::remove_file(&client).unwrap();
    std::fs::remove_file(&required_library).unwrap();
    let missing_files = api.get(&preflight).await;
    let files_absent = [&client, &required_library].map(|path| {
        std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    });
    std::fs::write(&client, &original).unwrap();
    std::fs::write(&required_library, &original_library).unwrap();
    let client_after_restore = std::fs::read(&client).unwrap();
    let library_after_restore = std::fs::read(&required_library).unwrap();
    let metadata = library.join(format!("versions/{VERSION}/{VERSION}.json"));
    let original_metadata = std::fs::read(&metadata).unwrap();
    let mut changed_metadata = original_metadata.clone();
    changed_metadata[0] ^= 1;
    std::fs::remove_file(&required_library).unwrap();
    std::fs::write(&metadata, &changed_metadata).unwrap();
    let invalid_metadata = api.get(&preflight).await;
    let library_absent_with_invalid_metadata = std::fs::symlink_metadata(&required_library)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let metadata_after_preflight = std::fs::read(&metadata).unwrap();
    std::fs::write(&required_library, &original_library).unwrap();
    std::fs::write(&metadata, &original_metadata).unwrap();
    let library_after_metadata_restore = std::fs::read(&required_library).unwrap();
    let metadata_after_restore = std::fs::read(&metadata).unwrap();
    let log_config = library.join("assets/log_configs/fixture-log.xml");
    let original_log_config = std::fs::read(&log_config).unwrap();
    assert_eq!(
        original_log_config,
        provider.state.routes["GET /artifacts/log.xml"]
    );
    std::fs::remove_file(&log_config).unwrap();
    std::fs::remove_file(&required_library).unwrap();
    let missing_libraries = api.get(&preflight).await;
    let libraries_absent = [&log_config, &required_library].map(|path| {
        std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    });
    std::fs::write(&log_config, &original_log_config).unwrap();
    std::fs::write(&required_library, &original_library).unwrap();
    let log_config_after_restore = std::fs::read(&log_config).unwrap();
    let library_after_log_restore = std::fs::read(&required_library).unwrap();
    let mut changed_log_config = original_log_config.clone();
    changed_log_config[0] ^= 1;
    std::fs::write(&log_config, &changed_log_config).unwrap();
    let corrupt_log_config = api.get(&preflight).await;
    let log_config_after_preflight = std::fs::read(&log_config).unwrap();
    std::fs::write(&log_config, &original_log_config).unwrap();
    std::fs::remove_file(&log_config).unwrap();
    let missing_log_config = api.get(&preflight).await;
    let log_config_absent = std::fs::symlink_metadata(&log_config)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let log_canary = temporary.path().join("outside-log-canary.xml");
    assert!(!log_canary.starts_with(&profile) && !log_canary.starts_with(&library));
    std::fs::write(&log_canary, &original_log_config).unwrap();
    std::os::unix::fs::symlink(&log_canary, &log_config).unwrap();
    let log_link_before = std::fs::symlink_metadata(&log_config).unwrap();
    let log_inadmissible = api.get(&preflight).await;
    let log_link_after = std::fs::symlink_metadata(&log_config).unwrap();
    let log_link_target = std::fs::read_link(&log_config).unwrap();
    let log_canary_after = std::fs::read(&log_canary).unwrap();
    std::fs::remove_file(&log_config).unwrap();
    std::fs::write(&log_config, &original_log_config).unwrap();
    let asset_index = library.join("assets/indexes/fixture-assets.json");
    let original_index = std::fs::read(&asset_index).unwrap();
    assert_eq!(
        original_index,
        provider.state.routes["GET /assets/index.json"]
    );
    let mut changed_index = original_index.clone();
    changed_index[0] ^= 1;
    std::fs::write(&asset_index, &changed_index).unwrap();
    let corrupt_index = api.get(&preflight).await;
    let index_after_preflight = std::fs::read(&asset_index).unwrap();
    std::fs::remove_file(&asset_index).unwrap();
    let missing_index = api.get(&preflight).await;
    let index_absent = std::fs::symlink_metadata(&asset_index)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let index_canary = temporary.path().join("outside-index-canary.json");
    assert!(!index_canary.starts_with(&profile) && !index_canary.starts_with(&library));
    std::fs::write(&index_canary, &original_index).unwrap();
    std::os::unix::fs::symlink(&index_canary, &asset_index).unwrap();
    let index_link_before = std::fs::symlink_metadata(&asset_index).unwrap();
    let index_inadmissible = api.get(&preflight).await;
    let index_link_after = std::fs::symlink_metadata(&asset_index).unwrap();
    let index_link_target = std::fs::read_link(&asset_index).unwrap();
    let index_canary_after = std::fs::read(&index_canary).unwrap();
    std::fs::remove_file(&asset_index).unwrap();
    std::fs::write(&asset_index, &original_index).unwrap();
    let asset_hash = sha1(ASSET);
    let asset_path = format!("assets/objects/{}/{asset_hash}", &asset_hash[..2]);
    let asset = library.join(&asset_path);
    let original_asset = std::fs::read(&asset).unwrap();
    assert_eq!(original_asset, ASSET);
    assert_eq!(
        original_asset,
        provider.state.routes[&format!("GET /{asset_path}")]
    );
    std::fs::remove_file(&asset).unwrap();
    let missing_asset = api.get(&preflight).await;
    let asset_absent = std::fs::symlink_metadata(&asset)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    std::fs::write(&asset, &original_asset).unwrap();
    let mut changed_library = original_library.clone();
    changed_library[0] ^= 1;
    std::fs::write(&required_library, &changed_library).unwrap();
    let corrupt_library = api.get(&preflight).await;
    let library_after_preflight = std::fs::read(&required_library).unwrap();
    std::fs::remove_file(&required_library).unwrap();
    let missing_library = api.get(&preflight).await;
    let library_absent = std::fs::symlink_metadata(&required_library)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let library_canary = temporary.path().join("outside-library-canary.jar");
    assert!(!library_canary.starts_with(&profile) && !library_canary.starts_with(&library));
    std::fs::write(&library_canary, &original_library).unwrap();
    std::os::unix::fs::symlink(&library_canary, &required_library).unwrap();
    let library_link_before = std::fs::symlink_metadata(&required_library).unwrap();
    let library_inadmissible = api.get(&preflight).await;
    let library_link_after = std::fs::symlink_metadata(&required_library).unwrap();
    let library_link_target = std::fs::read_link(&required_library).unwrap();
    let library_canary_after = std::fs::read(&library_canary).unwrap();
    std::fs::remove_file(&required_library).unwrap();
    std::fs::write(&required_library, &original_library).unwrap();
    std::fs::remove_file(&metadata).unwrap();
    let missing_metadata = api.get(&preflight).await;
    let metadata_absent = std::fs::symlink_metadata(&metadata)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let metadata_canary = temporary.path().join("outside-metadata-canary.json");
    assert!(!metadata_canary.starts_with(&profile) && !metadata_canary.starts_with(&library));
    std::fs::write(&metadata_canary, &original_metadata).unwrap();
    std::os::unix::fs::symlink(&metadata_canary, &metadata).unwrap();
    let metadata_link_before = std::fs::symlink_metadata(&metadata).unwrap();
    let metadata_inadmissible = api.get(&preflight).await;
    let metadata_link_after = std::fs::symlink_metadata(&metadata).unwrap();
    let metadata_link_target = std::fs::read_link(&metadata).unwrap();
    let metadata_canary_after = std::fs::read(&metadata_canary).unwrap();
    let client_after_metadata = std::fs::read(&client).unwrap();
    std::fs::remove_file(&metadata).unwrap();
    std::fs::write(&metadata, &original_metadata).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 1;
    std::fs::write(&client, &changed).unwrap();
    let corrupt = api.get(&preflight).await;
    let bytes_after_preflight = std::fs::read(&client).unwrap();
    std::fs::remove_file(&client).unwrap();
    let canary = temporary.path().join("outside-client-canary.jar");
    assert!(!canary.starts_with(&profile) && !canary.starts_with(&library));
    std::fs::write(&canary, &original).unwrap();
    std::os::unix::fs::symlink(&canary, &client).unwrap();
    let link_before = std::fs::symlink_metadata(&client).unwrap();
    let inadmissible = api.get(&preflight).await;
    let link_after = std::fs::symlink_metadata(&client).unwrap();
    let link_target = std::fs::read_link(&client).unwrap();
    std::fs::remove_file(&client).unwrap();
    let missing = api.get(&preflight).await;
    let canary_after = std::fs::read(&canary).unwrap();
    let log_config_after = std::fs::read(&log_config).unwrap();
    let metadata_after = std::fs::read(&metadata).unwrap();
    let library_after = std::fs::read(&required_library).unwrap();
    let index_after = std::fs::read(&asset_index).unwrap();
    let asset_after = std::fs::read(&asset).unwrap();
    let reports_after = api.get("/api/v1/launch/reports").await;
    let queue_after = api.get("/api/v1/install/queue").await;
    let sessions_after = api.get("/api/v1/launch/sessions").await;
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    assert_eq!(reports_before, json!({"reports":[]}));
    assert_eq!(reports_after, reports_before);
    assert_eq!(queue_after, queue_before);
    assert_eq!(sessions_before, json!({"sessions":[]}));
    assert_eq!(sessions_after, sessions_before);
    assert_eq!(
        files_absent,
        [true, true],
        "preflight must not repair files"
    );
    assert_eq!(client_after_restore, original);
    assert_eq!(library_after_restore, original_library);
    assert!(
        library_absent_with_invalid_metadata,
        "preflight must not repair libraries"
    );
    assert_eq!(changed_metadata.len(), original_metadata.len());
    assert_ne!(changed_metadata, original_metadata);
    assert_eq!(
        metadata_after_preflight, changed_metadata,
        "preflight must not repair metadata"
    );
    assert_eq!(library_after_metadata_restore, original_library);
    assert_eq!(metadata_after_restore, original_metadata);
    assert_eq!(
        libraries_absent,
        [true, true],
        "preflight must not repair libraries"
    );
    assert_eq!(log_config_after_restore, original_log_config);
    assert_eq!(library_after_log_restore, original_library);
    for (before, after, target, expected_target) in [
        (
            &log_link_before,
            &log_link_after,
            &log_link_target,
            &log_canary,
        ),
        (
            &index_link_before,
            &index_link_after,
            &index_link_target,
            &index_canary,
        ),
        (
            &library_link_before,
            &library_link_after,
            &library_link_target,
            &library_canary,
        ),
        (
            &metadata_link_before,
            &metadata_link_after,
            &metadata_link_target,
            &metadata_canary,
        ),
        (&link_before, &link_after, &link_target, &canary),
    ] {
        assert!(before.file_type().is_symlink());
        assert!(after.file_type().is_symlink());
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(target, expected_target);
    }
    assert_eq!(changed_log_config.len(), original_log_config.len());
    assert_ne!(changed_log_config, original_log_config);
    assert_eq!(
        log_config_after_preflight, changed_log_config,
        "preflight must not repair logging configuration"
    );
    assert!(
        log_config_absent,
        "preflight must not repair logging configuration"
    );
    assert_eq!(log_canary_after, original_log_config);
    assert_eq!(log_config_after, original_log_config);
    assert_eq!(changed_index.len(), original_index.len());
    assert_ne!(changed_index, original_index);
    assert_eq!(
        index_after_preflight, changed_index,
        "preflight must not repair the asset index"
    );
    assert!(index_absent, "preflight must not repair the asset index");
    assert_eq!(index_canary_after, original_index);
    assert_eq!(index_after, original_index);
    assert!(asset_absent, "preflight must not repair asset objects");
    assert_eq!(asset_after, original_asset);
    assert_eq!(changed_library.len(), original_library.len());
    assert_ne!(changed_library, original_library);
    assert_eq!(
        library_after_preflight, changed_library,
        "preflight must not repair libraries"
    );
    assert!(library_absent, "preflight must not repair libraries");
    assert_eq!(library_canary_after, original_library);
    assert_eq!(library_after, original_library);
    assert!(metadata_absent, "preflight must not repair metadata");
    assert_eq!(metadata_canary_after, original_metadata);
    assert_eq!(metadata_after, original_metadata);
    assert_eq!(client_after_metadata, original);
    assert_eq!(canary_after, original);
    for response in [
        invalid_metadata,
        log_inadmissible,
        missing_asset,
        index_inadmissible,
        library_inadmissible,
        metadata_inadmissible,
        inadmissible,
    ] {
        assert_eq!(
            response,
            json!({
                "instance_id":instance,"launchable":false,
                "error":{
                    "code":"install_unavailable",
                    "error":"The installed version requires a completed installation before launch."
                }
            })
        );
    }
    assert_eq!(changed.len(), original.len());
    assert_ne!(changed, original);
    assert_eq!(
        bytes_after_preflight, changed,
        "preflight must not repair files"
    );
    assert!(
        !client.try_exists().unwrap(),
        "preflight must not repair files"
    );
    let install_error = json!({
        "code":"install_unavailable",
        "error":"The installed version requires a completed installation before launch."
    });
    let responses = [
        (
            missing_libraries,
            "libraries_missing",
            "Required libraries are missing. Install this version before launching.",
        ),
        (
            corrupt_log_config,
            "libraries_corrupt",
            "Required libraries are corrupt. Repair this version before launching.",
        ),
        (
            missing_log_config,
            "libraries_missing",
            "Required libraries are missing. Install this version before launching.",
        ),
        (
            corrupt_index,
            "asset_index_corrupt",
            "Asset index is corrupt. Repair this version before launching.",
        ),
        (
            missing_index,
            "asset_index_missing",
            "Asset index is missing. Install this version before launching.",
        ),
        (
            corrupt_library,
            "libraries_corrupt",
            "Required libraries are corrupt. Repair this version before launching.",
        ),
        (
            missing_library,
            "libraries_missing",
            "Required libraries are missing. Install this version before launching.",
        ),
        (
            missing,
            "client_jar_missing",
            "Client game files are missing. Install this version before launching.",
        ),
        (
            corrupt,
            "client_jar_corrupt",
            "Client game files are corrupt. Repair this version before launching.",
        ),
    ]
    .map(|(response, reason, message)| {
        (
            response,
            json!([{"id":reason,"severity":"blocking","message":message}]),
            install_error.clone(),
        )
    });
    for (response, reasons, error) in [(
        missing_files,
        json!([
            {
                "id":"client_jar_missing","severity":"blocking",
                "message":"Client game files are missing. Install this version before launching."
            },
            {
                "id":"libraries_missing","severity":"blocking",
                "message":"Required libraries are missing. Install this version before launching."
            }
        ]),
        install_error,
    )]
    .into_iter()
    .chain(responses)
    .chain([(
        missing_metadata,
        json!([
            {
                "id":"installed_versions_degraded","severity":"blocking",
                "message":"Could not verify installed versions. Check the library folder and try again."
            },
            {
                "id":"version_json_missing","severity":"blocking",
                "message":"Installed version metadata is missing. Install this version before launching."
            }
        ]),
        json!({
            "code":"installed_versions_degraded",
            "error":"Could not verify installed versions. Check the library folder and try again."
        }),
    )])
    {
        assert_eq!(response["instance_id"], instance);
        assert_eq!(response["launchable"], false, "{response}");
        assert_eq!(response["error"], error, "{response}");
        assert_eq!(response["status"], "ready", "{response}");
        let mut readiness = response["readiness"].clone();
        readiness["reasons"]
            .as_array_mut()
            .expect("readiness reasons")
            .sort_unstable_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
        assert_eq!(readiness, json!({"launchable":false,"reasons":reasons}));
        assert_eq!(
            response["memory"],
            json!({"max_memory_mb":2048,"min_memory_mb":1024,"min_clamped":false})
        );
        assert_eq!(
            response["overrides"],
            json!({
                "java":{"present":false},"preset":{"present":false},
                "raw_jvm_args":{"present":true,"origin":"instance"}
            })
        );
        assert_eq!(response.as_object().unwrap().len(), 8);
        let budget = response["resource_budget"]
            .as_object()
            .expect("safe resource budget");
        assert_eq!(budget.len(), 9);
        assert_eq!(budget["active_session_count"], 0);
        assert_eq!(budget["active_install_count"], 0);
        assert_eq!(budget["active_memory_allocation_mb"], 0);
        assert_eq!(budget["requested_memory_mb"], 2048);
        let remaining = budget.get("estimated_remaining_memory_mb").unwrap();
        assert!(remaining.is_null() || remaining.as_i64().is_some());
        for pressure in [
            "memory_pressure",
            "cpu_pressure",
            "install_pressure",
            "disk_pressure",
        ] {
            assert!(budget[pressure].is_boolean(), "{pressure}: {budget:?}");
        }
        assert_eq!(budget["install_pressure"], false);
        let encoded = response.to_string();
        for private in [
            profile.to_str().unwrap(),
            log_config.to_str().unwrap(),
            log_canary.to_str().unwrap(),
            client.to_str().unwrap(),
            metadata.to_str().unwrap(),
            metadata_canary.to_str().unwrap(),
            required_library.to_str().unwrap(),
            library_canary.to_str().unwrap(),
            asset_index.to_str().unwrap(),
            index_canary.to_str().unwrap(),
            asset.to_str().unwrap(),
            private_args,
            "missing-client-secret",
            &api.capability,
        ] {
            assert!(
                !encoded.contains(private),
                "preflight exposed private input"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_reports_missing_java_override_without_launching() {
    use std::os::unix::fs::PermissionsExt;

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let missing_java = temporary.path().join("private-missing-java");
    let broken_java = temporary.path().join("private-java-script");
    let script = format!("#!{}\nexit 0\n", missing_java.display());
    std::fs::write(&broken_java, &script).unwrap();
    std::fs::set_permissions(&broken_java, std::fs::Permissions::from_mode(0o755)).unwrap();
    let non_executable_java = temporary.path().join("private-java-data");
    let data = b"preserve non-executable Java fixture";
    std::fs::write(&non_executable_java, data).unwrap();
    std::fs::set_permissions(&non_executable_java, std::fs::Permissions::from_mode(0o600)).unwrap();
    let canary = temporary.path().join("runtime-canary.bin");
    let canary_bytes = b"preserve unrelated runtime canary";
    std::fs::write(&canary, canary_bytes).unwrap();
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":0,"performance_mode":"vanilla","jvm_preset":"",
            "java_path_override":"","max_memory_mb":2048,"min_memory_mb":1024
        })),
    )
    .await;
    let start = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    assert_eq!(install_terminal(&api, &start).await["outcome"], "succeeded");
    let created = api
        .post(
            "/api/v1/instances",
            json!({"name":"Missing Java preflight","selection_id":format!("vanilla|{VERSION}")}),
        )
        .await;
    let instance = created["id"].as_str().unwrap();
    let instance_path = format!("/api/v1/instances/{instance}");
    let preflight = format!("/api/v1/launch/preflight/{instance}");
    let ready = api.get(&preflight).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(ready["launchable"], true, "{ready}");
    assert_eq!(ready["readiness"], json!({"launchable":true,"reasons":[]}));
    let reports_before = api.get("/api/v1/launch/reports").await;
    let queue_before = api.get("/api/v1/install/queue").await;
    let sessions_before = api.get("/api/v1/launch/sessions").await;
    let private_args = "-Dpreflight.private=java-override-secret";
    let mut responses = Vec::new();
    for java in [&missing_java, &broken_java, &non_executable_java] {
        let current = api.get(&instance_path).await;
        api.request(
            reqwest::Method::PUT,
            &instance_path,
            Some(json!({
                "expected_revision":current["revision"],"java_path":java,
                "extra_jvm_args":private_args
            })),
        )
        .await;
        responses.push(api.get(&preflight).await);
    }
    let script_after = std::fs::read(&broken_java).unwrap();
    let data_after = std::fs::read(&non_executable_java).unwrap();
    let canary_after = std::fs::read(&canary).unwrap();
    let reports_after = api.get("/api/v1/launch/reports").await;
    let queue_after = api.get("/api/v1/install/queue").await;
    let sessions_after = api.get("/api/v1/launch/sessions").await;
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    assert_eq!(reports_before, json!({"reports":[]}));
    assert_eq!(reports_after, reports_before);
    assert_eq!(queue_after, queue_before);
    assert_eq!(sessions_before, json!({"sessions":[]}));
    assert_eq!(sessions_after, sessions_before);
    assert_eq!(script_after, script.as_bytes());
    assert_eq!(data_after, data);
    assert_eq!(canary_after, canary_bytes);
    assert!(!missing_java.try_exists().unwrap());
    assert_eq!(
        responses[1],
        json!({
            "instance_id":instance,"launchable":false,
            "error":{
                "code":"runtime_unavailable","error":"The selected Java executable could not run."
            }
        })
    );
    for (response, error) in [
        (&responses[0], "The selected Java executable is missing."),
        (
            &responses[2],
            "The selected Java file is not an executable regular file.",
        ),
    ] {
        assert_eq!(response["instance_id"], instance);
        assert_eq!(response["launchable"], false, "{response}");
        assert_eq!(
            response["error"],
            json!({"code":"runtime_unavailable","error":error})
        );
        assert_eq!(response["status"], "ready", "{response}");
        assert_eq!(
            response["readiness"],
            json!({"launchable":false,"reasons":[{
                "id":"java_override_missing","severity":"blocking",
                "message":"Selected Java override is unavailable. Choose another Java runtime."
            }]})
        );
        assert_eq!(
            response["memory"],
            json!({"max_memory_mb":2048,"min_memory_mb":1024,"min_clamped":false})
        );
        assert_eq!(
            response["overrides"],
            json!({
                "java":{"present":true,"origin":"instance"},"preset":{"present":false},
                "raw_jvm_args":{"present":true,"origin":"instance"}
            })
        );
        assert_eq!(response.as_object().unwrap().len(), 8);
        let budget = response["resource_budget"]
            .as_object()
            .expect("safe resource budget");
        assert_eq!(budget.len(), 9);
        assert_eq!(budget["active_session_count"], 0);
        assert_eq!(budget["active_install_count"], 0);
        assert_eq!(budget["active_memory_allocation_mb"], 0);
        assert_eq!(budget["requested_memory_mb"], 2048);
        let remaining = budget.get("estimated_remaining_memory_mb").unwrap();
        assert!(remaining.is_null() || remaining.as_i64().is_some());
        for pressure in [
            "memory_pressure",
            "cpu_pressure",
            "install_pressure",
            "disk_pressure",
        ] {
            assert!(budget[pressure].is_boolean(), "{pressure}: {budget:?}");
        }
        assert_eq!(budget["install_pressure"], false);
        let encoded = response.to_string();
        for private in [
            temporary.path().to_str().unwrap(),
            profile.to_str().unwrap(),
            missing_java.to_str().unwrap(),
            broken_java.to_str().unwrap(),
            non_executable_java.to_str().unwrap(),
            private_args,
            "java-override-secret",
            &api.capability,
        ] {
            assert!(
                !encoded.contains(private),
                "preflight exposed private input"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_offline_vanilla_install_launch_stop_and_restart() {
    offline_vanilla_journey(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_external_offline_vanilla_install_launch_stop_and_restart() {
    offline_vanilla_journey(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_play_reacquires_missing_default_runtime_after_reopen() {
    use futures_util::FutureExt;
    use std::os::unix::fs::MetadataExt;

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let mut services: Option<DesktopServices> = None;
    let mut processes = BTreeSet::new();
    let journey = std::panic::AssertUnwindSafe(async {
        services = Some(
            start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
                .await
                .unwrap(),
        );
        let initial = services.as_ref().unwrap();
        let api = Api::new(initial);
        api.post(
            "/api/v1/accounts/offline",
            json!({"username":PLAYER,"expected_selection_revision":0}),
        )
        .await;
        api.request(
            reqwest::Method::PUT,
            "/api/v1/config",
            Some(json!({
                "expected_revision":0,"performance_mode":"vanilla","java_path_override":""
            })),
        )
        .await;
        let install = api
            .post(
                "/api/v1/install/queue",
                json!({"kind":"vanilla","version_id":VERSION}),
            )
            .await;
        let terminal = install_terminal(&api, &install).await;
        assert_eq!(terminal["outcome"], "succeeded", "{terminal}");
        let install_path = format!(
            "/api/v1/install/{}/status",
            install["started_install"]["install_id"].as_str().unwrap()
        );
        let created = api
            .post(
                "/api/v1/instances",
                json!({"name":"Missing default runtime","selection_id":format!("vanilla|{VERSION}")}),
            )
            .await;
        assert!(created["install_queue"].is_null(), "{created}");
        let instance = created["id"].as_str().unwrap();
        wait_launchable(&api, instance).await;
        assert_installed(&api, true).await;
        let queue_before_restart = api.get("/api/v1/install/queue").await;
        assert_eq!(queue_before_restart["items"], json!([]));
        let library = initial.library.admit().unwrap().read_projection().unwrap();
        let record = initial
            .instances
            .registry()
            .get_live(&instance.parse().unwrap())
            .unwrap();
        let save = library
            .join("instances")
            .join(record.directory_name)
            .join("saves/user-level.dat");
        std::fs::create_dir_all(save.parent().unwrap()).unwrap();
        std::fs::write(&save, b"user-owned save survives runtime provisioning").unwrap();
        let asset_hash = sha1(ASSET);
        let protected: Vec<_> = [
            save,
            library.join(format!("versions/{VERSION}/{VERSION}.json")),
            library.join(format!("versions/{VERSION}/{VERSION}.jar")),
            library.join("libraries/org/axial/fixture/1.0/fixture-1.0.jar"),
            library.join("assets/log_configs/fixture-log.xml"),
            library.join("assets/indexes/fixture-assets.json"),
            library.join(format!("assets/objects/{}/{asset_hash}", &asset_hash[..2])),
        ]
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
        let runtime = initial.installs.runtime_cache().root().join(COMPONENT);
        assert!(runtime.starts_with(&profile));
        assert_eq!(runtime.file_name().unwrap(), COMPONENT);
        let runtime_files: Vec<_> = [
            java_relative_path().to_owned(),
            java_relative_path().replace("/java", "/fake_java.py"),
            ".axial-runtime-manifest.json".to_owned(),
            ".axial-ready".to_owned(),
        ]
        .into_iter()
        .map(|relative| {
            let path = runtime.join(relative);
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
        assert_eq!(runtime_files[0].1, provider.state.routes["GET /java-runtime/java"]);
        assert_eq!(runtime_files[1].1, provider.state.routes["GET /java-runtime/fake_java.py"]);
        initial.server.shutdown().await.unwrap();
        assert!(initial.server.is_shutdown_settled());
        drop(services.take());
        assert!(std::fs::symlink_metadata(&runtime).unwrap().is_dir());
        assert_eq!(std::fs::canonicalize(&runtime).unwrap(), runtime);
        std::fs::remove_dir_all(&runtime).unwrap();
        assert!(!runtime.try_exists().unwrap());

        services = Some(
            start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
                .await
                .unwrap(),
        );
        let reopened = services.as_ref().unwrap();
        let restarted_api = Api::new(reopened);
        let queue_before = restarted_api.get("/api/v1/install/queue").await;
        assert_eq!(queue_before["items"], queue_before_restart["items"]);
        assert_eq!(queue_before["view_model"], queue_before_restart["view_model"]);
        assert_ne!(api.capability, restarted_api.capability);
        assert_eq!(restarted_api.get("/api/v1/config").await["java_path_override"], "");
        let preflight = format!("/api/v1/launch/preflight/{instance}");
        let readonly_requests_before = provider.state.requests.lock().unwrap().clone();
        let preflight_before = restarted_api.get(&preflight).await;
        let instance_path = format!("/api/v1/instances/{instance}");
        let instance_before = restarted_api.get(&instance_path).await;
        assert!(!runtime.try_exists().unwrap());
        let original_args = reopened.instances.registry()
            .get_live(&instance.parse().unwrap()).unwrap().instance.settings.extra_jvm_args;
        restarted_api.request(reqwest::Method::PUT, &instance_path, Some(json!({
            "expected_revision":instance_before["revision"],
            "extra_jvm_args":"-javaagent:fixture-agent.jar"
        }))).await;
        let invalid_plan = restarted_api.get(&preflight).await;
        let invalid_instance = restarted_api.get(&instance_path).await;
        let runtime_absent_after_invalid_plan = std::fs::symlink_metadata(&runtime)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let restored_instance = restarted_api.request(reqwest::Method::PUT, &instance_path, Some(json!({
            "expected_revision":invalid_instance["revision"],"extra_jvm_args":original_args
        }))).await;
        let restored_args = reopened.instances.registry()
            .get_live(&instance.parse().unwrap()).unwrap().instance.settings.extra_jvm_args;
        let readonly_requests_after = provider.state.requests.lock().unwrap().clone();
        let request_count = provider.state.requests.lock().unwrap().len();
        let response = restarted_api
            .client
            .post(format!("{}/api/v1/launch", restarted_api.base))
            .header(transport::CAPABILITY_HEADER, &restarted_api.capability)
            .json(&json!({"instance_id":instance,"intent_key":uuid::Uuid::new_v4().to_string()}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        let mut verify_guards = None;
        if status.is_success() {
            let session = body["session_id"].as_str().unwrap();
            processes.extend(observe_and_stop_session(&restarted_api, session).await);
            let report = restarted_api.get(&format!("/api/v1/launch/reports/{session}")).await;
            assert_eq!(report["instance_id"], instance);
            assert_eq!(report["session_outcome"]["kind"], "stopped");
            assert_eq!(report["session_outcome"]["reason"], "launcher_stopped");
            assert!(reopened.installs.runtime_cache().admit_component(COMPONENT)
                .unwrap().unwrap().contents_verified());
            for (path, bytes) in &runtime_files {
                assert_eq!(std::fs::read(path).unwrap(), *bytes);
            }
            let requests = provider.state.requests.lock().unwrap();
            for expected in [
                "GET /java-runtime/all.json",
                "GET /java-runtime/component.json",
                "GET /java-runtime/java",
                "GET /java-runtime/fake_java.py",
            ] {
                assert!(requests[request_count..].iter().any(|request| request == expected),
                    "Play must reacquire {expected}: {:?}", &requests[request_count..]);
            }
            drop(requests);
            let ready = restarted_api.get(&preflight).await;
            assert_eq!(ready["status"], "ready", "{ready}");
            assert_eq!(ready["launchable"], true, "{ready}");
            assert_installed(&restarted_api, true).await;

            let sessions_before = restarted_api.get("/api/v1/launch/sessions").await;
            let reports_before = restarted_api.get("/api/v1/launch/reports").await;
            let requests_before = provider.state.requests.lock().unwrap().clone();
            let executable = &runtime_files[0].0;
            let original_java = runtime_files[0].1.clone();
            let mut changed_java = original_java.clone();
            changed_java[0] ^= 1;
            std::fs::write(executable, &changed_java).unwrap();
            let response = restarted_api.client
                .post(format!("{}/api/v1/launch", restarted_api.base))
                .header(transport::CAPABILITY_HEADER, &restarted_api.capability)
                .json(&json!({"instance_id":instance,"intent_key":uuid::Uuid::new_v4().to_string()}))
                .send().await.unwrap();
            let corrupt_status = response.status();
            let corrupt_body: Value = response.json().await.unwrap();
            if corrupt_status.is_success() {
                processes.extend(observe_and_stop_session(
                    &restarted_api, corrupt_body["session_id"].as_str().unwrap(),
                ).await);
            }
            let java_after_refusal = std::fs::read(executable).unwrap();
            std::fs::write(executable, &original_java).unwrap();
            let sessions_after_corruption = restarted_api.get("/api/v1/launch/sessions").await;
            let reports_after_corruption = restarted_api.get("/api/v1/launch/reports").await;

            let config = restarted_api.get("/api/v1/config").await;
            restarted_api.request(reqwest::Method::PUT, "/api/v1/config", Some(json!({
                "expected_revision":config["revision"],"java_path_override":COMPONENT
            }))).await;
            let preserved_runtime = temporary.path().join("preserved-runtime");
            assert!(std::fs::symlink_metadata(&preserved_runtime)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound));
            let runtime_before = std::fs::symlink_metadata(&runtime).unwrap();
            assert!(runtime_before.is_dir());
            assert_eq!(std::fs::canonicalize(&runtime).unwrap(), runtime);
            std::fs::rename(&runtime, &preserved_runtime).unwrap();
            let override_preflight = restarted_api.get(&preflight).await;
            let override_instance = restarted_api.get(&instance_path).await;
            let response = restarted_api.client
                .post(format!("{}/api/v1/launch", restarted_api.base))
                .header(transport::CAPABILITY_HEADER, &restarted_api.capability)
                .json(&json!({"instance_id":instance,"intent_key":uuid::Uuid::new_v4().to_string()}))
                .send().await.unwrap();
            let override_status = response.status();
            let override_body: Value = response.json().await.unwrap();
            if override_status.is_success() {
                processes.extend(observe_and_stop_session(
                    &restarted_api, override_body["session_id"].as_str().unwrap(),
                ).await);
            }
            let runtime_absent = std::fs::symlink_metadata(&runtime)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if runtime_absent {
                std::fs::rename(&preserved_runtime, &runtime).unwrap();
            }
            let runtime_after = std::fs::symlink_metadata(&runtime).unwrap();
            let current = restarted_api.get("/api/v1/config").await;
            let restored_config = restarted_api.request(reqwest::Method::PUT, "/api/v1/config", Some(json!({
                "expected_revision":current["revision"],"java_path_override":config["java_path_override"]
            }))).await;
            let sessions_after_override = restarted_api.get("/api/v1/launch/sessions").await;
            let reports_after_override = restarted_api.get("/api/v1/launch/reports").await;
            let requests_after = provider.state.requests.lock().unwrap().clone();
            verify_guards = Some(move || {
                assert_eq!(requests_after, requests_before, "refused Play must not download");
                assert_eq!(sessions_after_corruption, sessions_before);
                assert_eq!(sessions_after_override, sessions_before);
                assert_eq!(reports_after_corruption, reports_before);
                assert_eq!(reports_after_override, reports_before);
                assert_eq!(changed_java.len(), original_java.len());
                assert_ne!(changed_java, original_java);
                assert_eq!(java_after_refusal, changed_java, "Play must not repair existing Java");
                assert!(runtime_absent, "explicit Java selection must not provision a fallback");
                assert_eq!((runtime_after.dev(), runtime_after.ino()), (runtime_before.dev(), runtime_before.ino()));
                assert_eq!(current["java_path_override"], COMPONENT);
                assert_eq!(restored_config["java_path_override"], config["java_path_override"]);
                for (path, bytes) in runtime_files {
                    assert_eq!(std::fs::read(path).unwrap(), bytes);
                }
                assert!(runtime_absent_after_invalid_plan, "invalid plan reads must not acquire Java");
                assert_eq!(restored_args, original_args);
                assert_eq!(restored_instance["extra_jvm_args"], "");
                assert_eq!(invalid_instance["extra_jvm_args"], "");
                assert_eq!(invalid_plan["launchable"], false, "{invalid_plan}");
                assert_eq!(invalid_plan["status"], Value::Null);
                assert_eq!(invalid_plan["readiness"], Value::Null);
                assert_eq!(invalid_plan["error"], json!({
                    "code":"plan_rejected",
                    "error":"The launch command could not be validated. Check the instance settings and installation."
                }));
                for blocked in [&invalid_instance, &override_instance] {
                    assert_eq!(blocked["launch_action"]["launchable"], false, "{blocked}");
                    assert_eq!(blocked["launch_action"]["primary_action"], "blocked");
                }
                assert_eq!(override_preflight["status"], "ready", "{override_preflight}");
                assert_eq!(override_preflight["launchable"], false);
                assert_eq!(override_preflight["overrides"]["java"], json!({"present":true,"origin":"global"}));
                assert_eq!(override_preflight["readiness"], json!({
                    "launchable":false,"reasons":[{
                        "id":"java_override_missing","severity":"blocking",
                        "message":"Selected Java override is unavailable. Choose another Java runtime."
                    }]
                }));
                for (status, body, message) in [
                    (corrupt_status, corrupt_body, "The selected Java executable changed. Select it again."),
                    (override_status, override_body, "The selected Java executable is missing."),
                ] {
                    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{body}");
                    assert_eq!(body, json!({"code":"runtime_unavailable","error":message}));
                }
            });
        }
        assert_eq!(restarted_api.get("/api/v1/install/queue").await, queue_before);
        assert_eq!(restarted_api.get(&install_path).await, terminal);
        provider.assert_requests(false);
        (status, body, preflight_before, instance_before, protected, verify_guards,
            readonly_requests_before, readonly_requests_after)
    })
    .catch_unwind()
    .await;
    let shutdown = match &services {
        Some(services) => {
            std::panic::AssertUnwindSafe(services.server.shutdown())
                .catch_unwind()
                .await
        }
        None => Ok(Ok(())),
    };
    let settled = services
        .as_ref()
        .is_none_or(|services| services.server.is_shutdown_settled());
    drop(services);
    let provider_shutdown = std::panic::AssertUnwindSafe(provider.shutdown())
        .catch_unwind()
        .await;
    let absent = std::panic::AssertUnwindSafe(assert_fixture_processes_gone(&processes))
        .catch_unwind()
        .await;
    let verified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shutdown
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .unwrap();
        assert!(settled);
        provider_shutdown.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        absent.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let (
            status,
            body,
            preflight_before,
            instance_before,
            protected,
            verify_guards,
            readonly_requests_before,
            readonly_requests_after,
        ) = journey.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        for (path, bytes) in protected {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert!(
            status.is_success(),
            "ordinary Play must reacquire missing default Java: {status}: {body}; preflight={preflight_before}; launch_action={}",
            instance_before["launch_action"]
        );
        verify_guards.expect("successful Play must exercise refusal guards")();
        assert_eq!(
            readonly_requests_after, readonly_requests_before,
            "readiness must not acquire provider data"
        );
        assert_eq!(preflight_before["status"], "ready", "{preflight_before}");
        assert_eq!(preflight_before["launchable"], true, "{preflight_before}");
        assert_eq!(preflight_before["error"], Value::Null);
        assert_eq!(
            preflight_before["readiness"],
            json!({
                "launchable":true,
                "reasons":[{
                    "id":"managed_runtime_missing","severity":"recoverable",
                    "message":"Managed Java runtime is missing and will be prepared before launch."
                }]
            })
        );
        assert_eq!(
            instance_before["launch_action"]["launchable"], true,
            "{instance_before}"
        );
        assert_eq!(instance_before["launch_action"]["primary_action"], "launch");
        assert_eq!(instance_before["launch_action"]["label"], "Launch");
    }));
    if let Err(panic) = verified {
        eprintln!(
            "Retained missing-runtime fixture after failure: {}",
            temporary.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

async fn offline_vanilla_journey(existing: bool) {
    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let external = existing.then(|| configure_external(&profile));
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    if let Some(selection) = &external {
        assert_external_library(&services, &profile, selection);
    }
    let api = Api::new(&services);
    assert_installed(&api, false).await;
    let account = api
        .post(
            "/api/v1/accounts/offline",
            json!({"username":PLAYER,"expected_selection_revision":0}),
        )
        .await;
    assert_eq!(account["selection_revision"], 1);
    let config = api
        .request(
            reqwest::Method::PUT,
            "/api/v1/config",
            Some(json!({
                "expected_revision":0,"performance_mode":"vanilla","java_path_override":""
            })),
        )
        .await;
    assert_eq!(config["launch_auth_mode"], "offline");
    let start = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    let terminal = install_terminal(&api, &start).await;
    assert_eq!(terminal["outcome"], "succeeded", "{terminal}");
    assert_installed(&api, true).await;

    let created = api
        .post(
            "/api/v1/instances",
            json!({"name":"Offline journey","selection_id":format!("vanilla|{VERSION}"),"max_memory_mb":2048}),
        )
        .await;
    let instance = created["id"].as_str().unwrap().to_owned();
    assert!(created["install_queue"].is_null(), "{created}");
    assert_eq!(created["view_model"]["summary"], "Instance created.");
    let library = services.library.admit().unwrap().read_projection().unwrap();
    let record = services
        .instances
        .registry()
        .get_live(&instance.parse().unwrap())
        .unwrap();
    let save = library
        .join("instances")
        .join(record.directory_name)
        .join("saves/user-level.dat");
    std::fs::create_dir_all(save.parent().unwrap()).unwrap();
    std::fs::write(&save, b"user-owned save survives stop and restart").unwrap();
    let runtime = services.installs.runtime_cache().root().join(COMPONENT);
    let executable = runtime.join(java_relative_path());
    let installed_java = std::fs::read(&executable).unwrap();
    assert_eq!(
        installed_java,
        *provider.state.routes.get("GET /java-runtime/java").unwrap()
    );
    assert!(runtime.join(".axial-runtime-manifest.json").is_file());
    let first = launch_and_stop(&api, &instance).await;
    #[cfg(debug_assertions)]
    let first_command = api.get(&format!("/api/v1/launch/{first}/command")).await;
    #[cfg(debug_assertions)]
    let first_logs = api.get(&format!("/api/v1/launch/{first}/logs")).await;
    let first_report = api.get(&format!("/api/v1/launch/reports/{first}")).await;
    let budget = &first_report["resource_budget"];
    assert!(
        budget.is_object(),
        "launch must retain its resource snapshot"
    );
    assert_eq!(budget["requested_memory_mb"], 2048);
    assert_eq!(budget["active_session_count"], 0);
    assert_eq!(budget["active_install_count"], 0);
    assert_eq!(budget["active_memory_allocation_mb"], 0);
    assert_eq!(budget["memory_headroom_mb"], 2048);
    assert_eq!(budget["launch_disk_headroom_mb"], 2048);
    let device = &first_report["device"];
    assert_eq!(device["total_memory_mb"], budget["host_total_memory_mb"]);
    assert_eq!(device["cpu_threads"], budget["host_cpu_threads"]);
    if !budget["host_total_memory_mb"].is_null() || !budget["host_cpu_threads"].is_null() {
        assert!(matches!(
            device["tier"].as_str(),
            Some("low" | "mid" | "high")
        ));
    } else {
        assert_eq!(device["tier"], "unknown");
    }
    let removed = api
        .request(
            reqwest::Method::DELETE,
            &format!(
                "/api/v1/accounts/{}?expected_selection_revision={}&expected_account_revision={}",
                account["account"]["account_id"].as_str().unwrap(),
                account["selection_revision"].as_u64().unwrap(),
                account["account"]["account_revision"].as_u64().unwrap(),
            ),
            None,
        )
        .await;
    assert_eq!(removed["status"], "account_removed");
    let empty_accounts = api.get("/api/v1/accounts").await;
    assert_eq!(empty_accounts["accounts"], json!([]));
    assert_eq!(empty_accounts["active_account_id"], Value::Null);
    assert_eq!(empty_accounts["launch_auth_mode"], "offline");
    let offline_config = api.get("/api/v1/config").await;
    assert_eq!(offline_config["username"], PLAYER);
    assert_eq!(offline_config["launch_auth_mode"], "offline");
    let without_account = launch_and_stop(&api, &instance).await;
    assert_ne!(first, without_account);
    assert_eq!(api.get("/api/v1/accounts").await, empty_accounts);
    provider.assert_requests(true);
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    // No endpoint hook, live provider, injected ready state, or Java override.
    let reopened = start_in_profile(profile.clone(), None).await.unwrap();
    if let Some(selection) = &external {
        assert_external_library(&reopened, &profile, selection);
    }
    let restarted_api = Api::new(&reopened);
    assert_ne!(api.capability, restarted_api.capability);
    assert_eq!(
        restarted_api
            .get(&format!("/api/v1/launch/reports/{first}"))
            .await,
        first_report
    );
    assert_installed(&restarted_api, true).await;
    assert_eq!(restarted_api.get("/api/v1/accounts").await, empty_accounts);
    assert_eq!(
        restarted_api.get("/api/v1/config").await["username"],
        PLAYER
    );
    let second = launch_and_stop(&restarted_api, &instance).await;
    assert_ne!(first, second);
    assert_eq!(restarted_api.get("/api/v1/accounts").await, empty_accounts);
    assert_eq!(std::fs::read(executable).unwrap(), installed_java);
    assert_eq!(
        std::fs::read(save).unwrap(),
        b"user-owned save survives stop and restart"
    );
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
    if let Some((_, root)) = &external {
        assert_eq!(
            std::fs::read(root.join("user-canary.bin")).unwrap(),
            EXTERNAL_CANARY
        );
    }

    #[cfg(debug_assertions)]
    {
        // The child observes the executable-equivalent argv[0] plus passed arguments.
        // Assert after shutdown so a diagnostic-contract failure cannot strand work.
        let observed_count = first_logs["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["source"] == "stdout")
            .find_map(|entry| {
                entry["text"]
                    .as_str()?
                    .strip_prefix("Fixture command elements ")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .expect("the real fixture child must report its command element count");
        assert!((1..=16_385).contains(&observed_count));
        assert_eq!(first_command["command_redacted"], true);
        assert_eq!(first_command["command_arg_count"], observed_count);
        assert!(
            first_command
                == json!({
                    "session_id":first,
                    "command":vec!["<redacted>"; observed_count],
                    "command_redacted":true,
                    "command_arg_count":observed_count,
                    "java_path_present":true,
                }),
            "command inspection must contain only the retained redacted contract"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_clean_benchmarks_compare_configured_modes_after_reopen() {
    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start_with_java_exit(false, true).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":0,"performance_mode":"managed","java_path_override":""
        })),
    )
    .await;
    let start = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    assert_eq!(install_terminal(&api, &start).await["outcome"], "succeeded");
    let created = api
        .post(
            "/api/v1/instances",
            json!({
                "name":"Clean benchmark comparison","selection_id":format!("vanilla|{VERSION}"),
                "max_memory_mb":2048,"min_memory_mb":512
            }),
        )
        .await;
    let instance = created["id"].as_str().unwrap();
    let instance_path = format!("/api/v1/instances/{instance}");
    let mut reports = Vec::new();
    for (mode, benchmark_profile) in [
        ("vanilla", "vanilla_baseline"),
        ("managed", "managed_default"),
    ] {
        let current = api.get(&instance_path).await;
        let configured = api
            .request(
                reqwest::Method::PUT,
                &instance_path,
                Some(json!({
                    "expected_revision":current["revision"],"performance_mode":mode
                })),
            )
            .await;
        assert_eq!(configured["performance_mode"], mode);
        wait_launchable(&api, instance).await;
        let launched = api
            .post(
                "/api/v1/launch/benchmark",
                json!({
                    "instance_id":instance,"profile":benchmark_profile,
                    "run_type":"coldish","benchmark_mode":"release_validation"
                }),
            )
            .await;
        let session = launched["session_id"].as_str().unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = api.get(&format!("/api/v1/launch/{session}/status")).await;
                if status["phase"] == "exited" {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("fixture Java must exit naturally and publish its settled report");
        assert_eq!(terminal["instance_id"], instance);
        assert_eq!(terminal["tree_settled"], true);
        assert_eq!(terminal["output_drained"], true);
        assert_eq!(terminal["process_alive"], false);
        assert_eq!(terminal["boot_observed"], true);
        assert_eq!(terminal["exit_code"], 0);
        assert_eq!(terminal["outcome"]["kind"], "clean", "{terminal}");
        assert_fixture_processes_gone(&BTreeSet::from([terminal["pid"].as_u64().unwrap()])).await;
        let report = api.get(&format!("/api/v1/launch/reports/{session}")).await;
        assert_eq!(report["session_id"], session);
        assert_eq!(report["instance_id"], instance);
        assert_eq!(report["outcome"], "exited");
        assert_eq!(report["session_outcome"]["kind"], "clean");
        assert_eq!(report["exit_code"], 0);
        assert!(report["boot_duration_ms"].as_u64().unwrap() > 0);
        let scenario = &report["scenario"];
        assert_eq!(scenario["performance_mode"], mode);
        assert_eq!(scenario["version_id"], VERSION);
        assert_eq!(scenario["requested_memory_mb"], 2048);
        assert_eq!(scenario["benchmark_profile"], benchmark_profile);
        assert_eq!(scenario["benchmark_run_type"], "coldish");
        assert_eq!(scenario["benchmark_mode"], "release_validation");
        assert!(matches!(
            report["device"]["tier"].as_str(),
            Some("low" | "mid" | "high")
        ));
        let logs = report["logs"].as_array().unwrap();
        assert!(
            logs.iter()
                .any(|line| line["text"] == "Fixture command validated")
        );
        assert!(
            logs.iter()
                .any(|line| line["text"] == "Fixture heap MiB 2048")
        );
        reports.push(report);
    }
    let baseline = &reports[0];
    let managed = &reports[1];
    assert_ne!(managed["session_id"], baseline["session_id"]);
    assert!(baseline["comparison"].is_null());
    assert_eq!(managed["device"]["tier"], baseline["device"]["tier"]);
    let comparison = &managed["comparison"];
    assert_eq!(comparison["baseline_session_id"], baseline["session_id"]);
    assert_eq!(comparison["baseline_recorded_at"], baseline["recorded_at"]);
    assert_eq!(comparison["matched_sample_count"], 1);
    assert_eq!(comparison["metric_name"], "boot_duration_ms");
    assert_eq!(
        comparison["baseline_value_ms"],
        baseline["boot_duration_ms"]
    );
    assert_eq!(comparison["current_value_ms"], managed["boot_duration_ms"]);
    assert_eq!(
        comparison["baseline"],
        json!({
            "performance_mode":"vanilla","version_id":VERSION,"requested_memory_mb":2048,
            "device_tier":baseline["device"]["tier"],"benchmark_profile":"vanilla_baseline",
            "benchmark_run_type":"coldish","benchmark_mode":"release_validation"
        })
    );
    provider.assert_requests(true);
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    let reopened = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&reopened);
    assert_eq!(api.get(&instance_path).await["performance_mode"], "managed");
    for report in reports {
        let session = report["session_id"].as_str().unwrap();
        assert_eq!(
            api.get(&format!("/api/v1/launch/reports/{session}")).await,
            report
        );
    }
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_playing_instance_edits_preserve_active_command_and_next_launch_settings() {
    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({"expected_revision":0,"performance_mode":"vanilla"})),
    )
    .await;
    let start = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    assert_eq!(install_terminal(&api, &start).await["outcome"], "succeeded");
    let created = api
        .post(
            "/api/v1/instances",
            json!({
                "name":"Playing edit","selection_id":format!("vanilla|{VERSION}"),
                "max_memory_mb":768,"min_memory_mb":256
            }),
        )
        .await;
    let instance = created["id"].as_str().unwrap();
    wait_launchable(&api, instance).await;
    let launched = api
        .post(
            "/api/v1/launch",
            json!({"instance_id":instance,"intent_key":uuid::Uuid::new_v4().to_string()}),
        )
        .await;
    let first = launched["session_id"].as_str().unwrap();
    let (_, processes) = observe_running_session(&api, first).await;
    let before = api.get(&format!("/api/v1/instances/{instance}")).await;
    assert!(!before["last_played_at"].as_str().unwrap().is_empty());
    let rename = api
        .client
        .put(format!("{}/api/v1/instances/{instance}", api.base))
        .header(transport::CAPABILITY_HEADER, &api.capability)
        .json(&json!({"name":"Renamed while playing","expected_revision":before["revision"]}))
        .send()
        .await
        .unwrap();
    let rename_status = rename.status();
    let renamed: Value = rename.json().await.unwrap();
    if !rename_status.is_success() {
        // The intended red must still settle the real fixture process and API.
        observe_and_stop_session(&api, first).await;
        assert_fixture_processes_gone(&processes).await;
        services.server.shutdown().await.unwrap();
        drop(services);
        provider.shutdown().await;
        panic!("Playing metadata edit was refused: {rename_status}: {renamed}");
    }
    assert_eq!(renamed["name"], "Renamed while playing");
    let edited = api
        .request(
            reqwest::Method::PUT,
            &format!("/api/v1/instances/{instance}"),
            Some(json!({"max_memory_mb":1024,"expected_revision":renamed["revision"]})),
        )
        .await;
    assert_eq!(edited["max_memory_mb"], 1024);
    assert_eq!(edited["last_played_at"], before["last_played_at"]);
    for patch in [
        json!({"max_memory_mb":2048,"expected_revision":renamed["revision"]}),
        json!({"version_id":"another-version","expected_revision":edited["revision"]}),
    ] {
        let response = api
            .client
            .put(format!("{}/api/v1/instances/{instance}", api.base))
            .header(transport::CAPABILITY_HEADER, &api.capability)
            .json(&patch)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
    let deletion = api
        .client
        .delete(format!("{}/api/v1/instances/{instance}", api.base))
        .header(transport::CAPABILITY_HEADER, &api.capability)
        .send()
        .await
        .unwrap();
    assert_eq!(deletion.status(), StatusCode::CONFLICT);
    assert_eq!(
        api.get(&format!("/api/v1/launch/{first}/status")).await["phase"],
        "running"
    );
    observe_and_stop_session(&api, first).await;
    assert_fixture_processes_gone(&processes).await;
    let original = services.benchmarks.reports().get(first).unwrap().unwrap();
    assert_eq!(original.scenario.requested_memory_mb, Some(768));
    assert!(
        original
            .logs
            .iter()
            .any(|line| line.text == "Fixture heap MiB 768")
    );
    services.server.shutdown().await.unwrap();
    drop(services);
    provider.shutdown().await;

    let reopened = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&reopened);
    let restored = api.get(&format!("/api/v1/instances/{instance}")).await;
    assert_eq!(restored["name"], "Renamed while playing");
    assert_eq!(restored["max_memory_mb"], 1024);
    assert_eq!(restored["revision"], edited["revision"]);
    let second = launch_and_stop(&api, instance).await;
    let next = reopened.benchmarks.reports().get(&second).unwrap().unwrap();
    assert_eq!(next.scenario.requested_memory_mb, Some(1024));
    assert!(
        next.logs
            .iter()
            .any(|line| line.text == "Fixture heap MiB 1024")
    );
    assert_eq!(
        reopened.benchmarks.reports().get(first).unwrap().unwrap(),
        original
    );
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
}

#[test]
fn real_benchmark_mapping_survives_response_loss_and_restart() {
    use tracing_subscriber::prelude::*;

    thread_local! {
        static DIAGNOSTICS: std::cell::RefCell<Option<tracing::dispatcher::DefaultGuard>> =
            const { std::cell::RefCell::new(None) };
    }
    let diagnostics = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_ansi(false)
            .without_time()
            .finish()
            .with(
                tracing_subscriber::filter::Targets::new()
                    .with_target("axial_app::launch::session", tracing::Level::WARN)
                    .with_target("axial_app::launch::prepare", tracing::Level::WARN),
            ),
    );
    let _diagnostics = tracing::dispatcher::set_default(&diagnostics);
    // HTTP and retained launch tasks run on this test's runtime threads.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(move || {
            DIAGNOSTICS.with(|guard| {
                *guard.borrow_mut() = Some(tracing::dispatcher::set_default(&diagnostics));
            });
        })
        .on_thread_stop(|| {
            DIAGNOSTICS.with(|guard| drop(guard.borrow_mut().take()));
        })
        .build()
        .unwrap();
    runtime.block_on(benchmark_mapping_survives_response_loss_and_restart());
}

async fn benchmark_mapping_survives_response_loss_and_restart() {
    use axial_app::storage::StorageError;
    use axial_app::{
        launch::reports::LaunchProofScenario, performance::benchmarks::BenchmarkLaunchRequest,
    };
    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    api.post(
        "/api/v1/accounts/offline",
        json!({"username":PLAYER,"expected_selection_revision":0}),
    )
    .await;
    api.request(
        reqwest::Method::PUT,
        "/api/v1/config",
        Some(json!({
            "expected_revision":0,"performance_mode":"vanilla","java_path_override":""
        })),
    )
    .await;
    let installation = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    let terminal = install_terminal(&api, &installation).await;
    assert_eq!(terminal["outcome"], "succeeded", "{terminal}");
    let created = api
        .post(
            "/api/v1/instances",
            json!({
                "name":"Benchmark recovery","selection_id":format!("vanilla|{VERSION}")
            }),
        )
        .await;
    let instance = created["id"].as_str().unwrap().to_owned();
    assert!(created["install_queue"].is_null(), "{created}");
    assert_eq!(created["view_model"]["summary"], "Instance created.");
    wait_launchable(&api, &instance).await;

    // Refuse only the response-side update after a real session was accepted.
    // The pre-spawn mapping must already be durable when this write fails.
    services.instances.registry().storage().transaction(|tx| -> Result<(), StorageError> {
        tx.execute_batch("CREATE TRIGGER refuse_benchmark_response BEFORE UPDATE ON benchmark_suites WHEN instr(CAST(NEW.payload AS TEXT),'\"state\":\"running\"')>0 BEGIN SELECT RAISE(ABORT,'response write unavailable'); END;")?;
        Ok(())
    }).unwrap();
    let request =
        json!({"instance_id":instance,"suite_id":"recovery-suite","suite_mode":"development"});
    let response = api
        .client
        .post(format!("{}/api/v1/launch/benchmark/suite/tick", api.base))
        .header(transport::CAPABILITY_HEADER, &api.capability)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let suite = api
        .get("/api/v1/launch/benchmark/suites/recovery-suite")
        .await;
    assert_eq!(suite["runs"][0]["state"], "launching", "{suite}");
    let first = suite["runs"][0]["session_id"]
        .as_str()
        .expect("mapping precedes accepted process")
        .to_owned();
    let first_intent = suite["runs"][0]["launch_intent"].as_str().unwrap();
    let accepted = api
        .get(&format!("/api/v1/launch/intents/{first_intent}"))
        .await;
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(accepted["session"]["session_id"], first);
    assert_eq!(accepted["session"]["instance_id"], instance);
    let retried = api
        .post("/api/v1/launch/benchmark/suite/tick", request.clone())
        .await;
    assert_eq!(retried["state"], "active");
    assert_eq!(retried["active_session_id"], first);
    services
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TRIGGER refuse_benchmark_response")?;
            Ok(())
        })
        .unwrap();
    observe_and_stop_session(&api, &first).await;
    let proof = services.benchmarks.reports().get(&first).unwrap().unwrap();
    assert_eq!(proof.instance_id, instance);
    assert_eq!(
        proof.scenario.benchmark_profile.as_deref(),
        Some("vanilla_baseline")
    );
    let other = api
        .post(
            "/api/v1/instances",
            json!({
                "name":"Other selected instance","selection_id":format!("vanilla|{VERSION}")
            }),
        )
        .await;
    assert!(other["install_queue"].is_null(), "{other}");
    assert_eq!(other["view_model"]["summary"], "Instance created.");
    let other_instance = other["id"].as_str().unwrap();
    let pending_input: BenchmarkLaunchRequest = serde_json::from_value(json!({
        "instance_id":instance,"suite_id":"pending-suite","suite_mode":"development"
    }))
    .unwrap();
    let mut pending_suite = services.benchmarks.ensure_suite(&pending_input).unwrap();
    let pending_session = uuid::Uuid::new_v4().to_string();
    let pending_run = &mut pending_suite.runs[0];
    pending_run.state = "launching".into();
    pending_run.session_id = Some(pending_session.clone());
    let context = serde_json::to_string(&LaunchProofScenario {
        benchmark_id: Some(pending_run.benchmark_id.clone()),
        benchmark_profile: Some(pending_run.profile.clone()),
        benchmark_run_type: Some(pending_run.run_type.clone()),
        benchmark_mode: Some("development".into()),
        ..LaunchProofScenario::default()
    })
    .unwrap();
    let intent = json!({"request": {
        "instance_id":instance,"version_id":null,"username":PLAYER,
        "max_memory_mb":768,"min_memory_mb":256,"client_started_at_ms":null,
        "intent_key":pending_run.launch_intent
    }, "context":context,"session_id":pending_session});
    // Durable fixture at the mapping-before-preparation crash boundary. There
    // is deliberately no accepted process or terminal report for this identity.
    services
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute(
                "INSERT INTO launch_intents(intent_key,payload,state) VALUES(?1,?2,'pending')",
                axial_app::storage::rusqlite::params![
                    pending_suite.runs[0].launch_intent,
                    serde_json::to_vec(&intent).unwrap()
                ],
            )?;
            tx.execute(
                "UPDATE benchmark_suites SET payload=?1 WHERE suite_id='pending-suite'",
                [serde_json::to_vec(&pending_suite).unwrap()],
            )?;
            tx.execute(
                "UPDATE instance_selection SET instance_id=?1 WHERE singleton=1",
                [other_instance],
            )?;
            Ok(())
        })
        .unwrap();
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    let reopened = start_in_profile(profile.clone(), None).await.unwrap();
    let api = Api::new(&reopened);
    assert_eq!(
        reopened
            .instances
            .registry()
            .last_instance_id()
            .unwrap()
            .unwrap()
            .as_str(),
        other_instance
    );
    let historical = api
        .get(&format!("/api/v1/launch/intents/{first_intent}"))
        .await;
    assert_eq!(historical["state"], "accepted");
    assert_eq!(historical["session"]["phase"], "exited");
    assert_eq!(historical["session"]["outcome"]["kind"], "stopped");
    assert_eq!(historical["session"]["session_id"], first);
    let pending_response = api.client.post(format!("{}/api/v1/launch/benchmark/suite/tick", api.base))
        .header(transport::CAPABILITY_HEADER, &api.capability)
        .json(&json!({
            "suite_id":"pending-suite","username":"ChangedPlayer","max_memory_mb":2048,"min_memory_mb":512
        })).send().await.unwrap();
    let pending_status = pending_response.status();
    let pending_body = pending_response.text().await.unwrap();
    assert!(
        pending_status.is_success(),
        "pending recovery: {pending_status}: {pending_body}; intent: {:?}",
        reopened
            .launch
            .intent(pending_suite.runs[0].launch_intent.as_deref().unwrap())
    );
    let pending: Value = serde_json::from_str(&pending_body).unwrap();
    assert_eq!(pending["state"], "launched", "{pending}");
    assert_eq!(pending["session"]["session_id"], pending_session);
    assert_eq!(pending["session"]["instance_id"], instance);
    assert_eq!(
        pending["suite"]["runs"][0]["launch_intent"],
        pending_suite.runs[0].launch_intent.as_deref().unwrap()
    );
    observe_and_stop_session(&api, &pending_session).await;
    let pending_proof = reopened
        .benchmarks
        .reports()
        .get(&pending_session)
        .unwrap()
        .unwrap();
    assert_eq!(pending_proof.scenario.requested_memory_mb, Some(768));
    wait_launchable(&api, &instance).await;
    let next = api
        .post(
            "/api/v1/launch/benchmark/suite/tick",
            json!({"suite_id":"recovery-suite"}),
        )
        .await;
    assert_eq!(next["state"], "launched", "{next}");
    assert_eq!(next["run_index"], 1);
    assert_eq!(next["suite"]["runs"][0]["state"], "stopped");
    assert_eq!(next["suite"]["runs"][0]["session_id"], first);
    assert_eq!(next["suite"]["runs"][0]["launched_at"], proof.launched_at);
    let second = next["session"]["session_id"].as_str().unwrap();
    assert_ne!(second, first);
    assert_eq!(next["suite"]["runs"][1]["session_id"], second);
    assert_eq!(next["session"]["instance_id"], instance);
    observe_and_stop_session(&api, second).await;
    let complete = api
        .post(
            "/api/v1/launch/benchmark/suite/tick",
            json!({"suite_id":"recovery-suite"}),
        )
        .await;
    assert_eq!(complete["state"], "complete");

    wait_launchable(&api, &instance).await;
    let observed = api
        .post(
            "/api/v1/launch/benchmark/suite/tick",
            json!({
                "instance_id":instance,"suite_id":"report-loss-suite","suite_mode":"development"
            }),
        )
        .await;
    let settled_session = observed["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let settled_intent = observed["suite"]["runs"][0]["launch_intent"]
        .as_str()
        .unwrap()
        .to_owned();
    observe_and_stop_session(&api, &settled_session).await;
    let settled = api
        .get(&format!("/api/v1/launch/intents/{settled_intent}"))
        .await;
    let saved_suite = api
        .get("/api/v1/launch/benchmark/suites/report-loss-suite")
        .await;
    // Loss of a diagnostic report does not erase the owner's actual settlement
    // observation or grant permission to repeat the benchmark run.
    reopened
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            assert_eq!(
                tx.execute(
                    "DELETE FROM launch_reports WHERE session_id=?1",
                    [&settled_session]
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
    drop(reopened);

    let reopened = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&reopened);
    let status = api
        .get(&format!("/api/v1/launch/intents/{settled_intent}"))
        .await;
    assert_observed_terminal(&status, &instance, &settled_session);
    assert_eq!(
        status["session"]["launched_at"],
        settled["session"]["launched_at"]
    );
    assert_eq!(
        status["session"]["exit_code"],
        settled["session"]["exit_code"]
    );
    for run_index in [Value::Null, json!(0)] {
        let result = api
            .client
            .post(format!("{}/api/v1/launch/benchmark/suite/tick", api.base))
            .header(transport::CAPABILITY_HEADER, &api.capability)
            .json(&json!({"suite_id":"report-loss-suite","run_index":run_index}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            result.json::<Value>().await.unwrap(),
            json!({
                "error":axial_app::performance::benchmarks::BenchmarkError::Unavailable.to_string()
            })
        );
        let retained = api
            .get("/api/v1/launch/benchmark/suites/report-loss-suite")
            .await;
        assert_eq!(retained, saved_suite);
        assert_eq!(retained["runs"][0]["session_id"], settled_session);
        assert_eq!(retained["runs"][1]["state"], "pending");
    }
    assert!(reopened.sessions.snapshots().is_empty());
    assert!(
        reopened
            .benchmarks
            .reports()
            .get(&settled_session)
            .unwrap()
            .is_none()
    );
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupted_client_cannot_publish_ready_installation() {
    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(true).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    let start = api
        .post(
            "/api/v1/install/queue",
            json!({"kind":"vanilla","version_id":VERSION}),
        )
        .await;
    let terminal = install_terminal(&api, &start).await;
    assert_eq!(terminal["outcome"], "failed", "{terminal}");
    assert_eq!(terminal["view_model"]["failed"], true, "{terminal}");
    assert_installed(&api, false).await;
    provider.assert_requests(false);
    assert!(
        provider
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request == "GET /artifacts/client.jar")
    );
    services.server.shutdown().await.unwrap();
    drop(services);
    provider.shutdown().await;
    let reopened = start_in_profile(profile, None).await.unwrap();
    assert_installed(&Api::new(&reopened), false).await;
    reopened.server.shutdown().await.unwrap();
}
