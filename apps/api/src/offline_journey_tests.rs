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

fn fake_java() -> (Vec<u8>, Vec<u8>) {
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
    let config = json!({
        "java_version":"17.0.12", "arch":std::env::consts::ARCH,
        "events":[
            {"stream":"stdout","text":"LWJGL Version: fixture\n"},
            {"stream":"stderr","text":"Fixture standard error\n"}
        ],
        "stall_ms":30000, "descendant_depth":1, "descendant_stall_ms":30000,
        "descendants_ignore_sigterm":true, "report_lifecycle":true
    });
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

fn provider_routes(base: &str, corrupt_client: bool) -> BTreeMap<String, Vec<u8>> {
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
    let (java, helper) = fake_java();
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
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = ProviderState {
            routes: Arc::new(provider_routes(&base, corrupt_client)),
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
async fn real_offline_vanilla_install_launch_stop_and_restart() {
    offline_vanilla_journey(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_external_offline_vanilla_install_launch_stop_and_restart() {
    offline_vanilla_journey(true).await;
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
