//! Deterministic integration, not real-JVM gameplay or installed-package proof.
//! Every install source comes over loopback HTTP and must pass the normal
//! downloader, managed-runtime, publication, queue and launch owners.

use super::{DesktopServices, start_in_profile, start_profile_with_test_endpoints, transport};
use axial_minecraft::download::InstallTestEndpoints;
use axum::{
    Router,
    extract::{OriginalUri, State},
    http::{Method, StatusCode},
};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

const VERSION: &str = "axial-offline-fixture";
const COMPONENT: &str = "java-runtime-gamma";
const ASSET: &[u8] = b"real downloaded fixture asset";
const NATIVE: &[u8] = b"fixture native bytes, not an executable native library";
const PLAYER: &str = "FixturePlayer";

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

const SETTLED_CHILD_PROFILE: &str = "AXIAL_TEST_SETTLED_REPORT_PROFILE";
const SETTLED_CHILD_INSTANCE: &str = "AXIAL_TEST_SETTLED_REPORT_INSTANCE";
const SETTLED_CHILD_INTENT: &str = "AXIAL_TEST_SETTLED_REPORT_INTENT";
const SETTLED_CHILD_EXIT: i32 = 74;

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
    use std::{
        io::{Read, Seek, SeekFrom},
        process::Stdio,
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
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
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
    let result = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    output
        .seek(SeekFrom::Start(
            output.metadata().unwrap().len().saturating_sub(65536),
        ))
        .unwrap();
    let mut diagnostics = Vec::new();
    output.read_to_end(&mut diagnostics).unwrap();
    let diagnostics = String::from_utf8_lossy(&diagnostics);
    let tail = diagnostics.lines().rev().take(30).collect::<Vec<_>>();
    assert!(result.is_ok(), "settlement helper timed out: {tail:?}");
    assert_eq!(
        result.unwrap().unwrap().code(),
        Some(SETTLED_CHILD_EXIT),
        "{tail:?}"
    );

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
    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let profile = temporary.path().join("profile");
    let provider = Provider::start(false).await;
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
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
            json!({"name":"Offline journey","selection_id":format!("vanilla|{VERSION}")}),
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
    provider.assert_requests(true);
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    // No endpoint hook, live provider, injected ready state, or Java override.
    let reopened = start_in_profile(profile, None).await.unwrap();
    let restarted_api = Api::new(&reopened);
    assert_ne!(api.capability, restarted_api.capability);
    assert_installed(&restarted_api, true).await;
    assert_eq!(
        reopened.accounts.capture_selected().unwrap().display_name(),
        PLAYER
    );
    let second = launch_and_stop(&restarted_api, &instance).await;
    assert_ne!(first, second);
    assert_eq!(std::fs::read(executable).unwrap(), installed_java);
    assert_eq!(
        std::fs::read(save).unwrap(),
        b"user-owned save survives stop and restart"
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

fn benchmark_predecessor_files(root: &Path, mixed: bool, queued: bool) -> Vec<(PathBuf, Vec<u8>)> {
    use axial_app::performance::benchmarks::{benchmark_suite_plan, benchmark_suite_run_id};

    const INSTANCE: &str = "0000000000000001";
    const SUITE: &str = "suite-dev-0000000000000001";
    const DRIVER: &str = "benchmark-suite-driver-0000000000000001";
    let mut instances: Value = serde_json::from_str(include_str!(
        "../../../acceptance/fixtures/profiles/offline-vanilla/instances.json"
    ))
    .unwrap();
    instances["instances"][0]["version_id"] = json!(VERSION);
    instances["instances"][0]["minecraft_version"] = json!(VERSION);
    let runs: Vec<_> = benchmark_suite_plan("development")
        .unwrap()
        .into_iter()
        .enumerate()
        .map(|(index, run)| {
            let inherited = mixed && index == 0;
            json!({
                "run_index":index,"profile":run.profile,"run_type":run.run_type,
                "target_id":run.target_id.unwrap_or(""),
                "benchmark_id":benchmark_suite_run_id("development", index, run),
                "state":if inherited { "exited" } else { "pending" },
                "session_id":inherited.then_some("session-a"),
                "launched_at":inherited.then_some("2026-01-01T00:00:00.000Z")
            })
        })
        .collect();
    assert_eq!(runs.len(), 2, "the retained development plan has two runs");
    let suite = json!({
        "schema":"axial.launch.benchmark.suite","schema_version":2,
        "suite_id":SUITE,"instance_id":INSTANCE,"mode":"development",
        "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z",
        "runs":runs
    });
    let driver = json!({
        "id":DRIVER,"suite_id":SUITE,"mode":"development","state":if queued { "interrupted" } else { "stopped" },
        "interval_ms":5000,"run_count":2,"launched_run_count":usize::from(mixed),
        "pending_run_index":usize::from(mixed),"active_session_id":null,
        "last_run_index":mixed.then_some(0),"last_session_id":mixed.then_some("session-a"),
        "error":if queued { "driver automatic resume queued after restart" } else { "Stopped by the user" },
        "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z"
    });
    let mut files = vec![
        (
            root.join("instances.json"),
            serde_json::to_vec(&instances).unwrap(),
        ),
        (
            root.join("config.json"),
            include_bytes!("../../../acceptance/fixtures/profiles/offline-vanilla/config.json")
                .to_vec(),
        ),
        (
            root.join("accounts.json"),
            include_bytes!("../../../acceptance/fixtures/profiles/offline-vanilla/accounts.json")
                .to_vec(),
        ),
        (
            root.join(format!("instances/{INSTANCE}/saves/user-level.dat")),
            b"predecessor save survives benchmark continuation".to_vec(),
        ),
        (
            root.join(format!("benchmarks/suites/{SUITE}.json")),
            serde_json::to_vec(&suite).unwrap(),
        ),
        (
            root.join(format!("benchmarks/suite-drivers/{DRIVER}.json")),
            serde_json::to_vec(&driver).unwrap(),
        ),
    ];
    if mixed {
        // A predecessor report is imported as historical evidence. New runs
        // below must acquire their reports from actual owned fixture processes.
        let report = json!({
            "schema":"axial.launch.proof","schema_version":3,
            "session_id":"session-a","instance_id":INSTANCE,"version_id":VERSION,
            "launched_at":"2026-01-01T00:00:00.000Z","recorded_at":"2026-01-01T00:00:02.000Z",
            "outcome":"exited",
            "session_outcome":{"reason":"clean_exit","kind":"clean","summary":"Minecraft exited cleanly."},
            "scenario":{"scenario_id":"vanilla_launch","performance_mode":"vanilla",
                "requested_memory_mb":2048,"version_id":VERSION,
                "benchmark_profile":runs[0]["profile"],"benchmark_run_type":runs[0]["run_type"],
                "benchmark_mode":"development","benchmark_id":runs[0]["benchmark_id"]},
            "device":{"tier":"mid","total_memory_mb":8192,"cpu_threads":8},
            "exit_code":0,"boot_duration_ms":1000,"stages":[]
        });
        files.push((
            root.join("benchmarks/launch/session-a.json"),
            serde_json::to_vec(&report).unwrap(),
        ));
    }
    for (path, bytes) in &files {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    files
}

fn retained_benchmark_bytes(
    services: &DesktopServices,
    suite: &str,
    driver: &str,
    report: Option<&str>,
) -> Vec<Vec<u8>> {
    services
        .instances
        .registry()
        .storage()
        .read(|db| {
            let suite: Vec<u8> = db.query_row(
                "SELECT payload FROM benchmark_suites WHERE suite_id=?1",
                [suite],
                |row| row.get(0),
            )?;
            let (driver, request): (Vec<u8>, Option<Vec<u8>>) = db.query_row(
                "SELECT payload,request FROM benchmark_drivers WHERE driver_id=?1",
                [driver],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert!(
                request.is_none(),
                "historical source never acquires a runnable request"
            );
            let mut bytes = vec![suite, driver];
            if let Some(report) = report {
                bytes.push(db.query_row(
                    "SELECT payload FROM launch_reports WHERE session_id=?1",
                    [report],
                    |row| row.get(0),
                )?);
            }
            Ok::<_, axial_app::storage::StorageError>(bytes)
        })
        .unwrap()
}

async fn imported_benchmark_resume_journey(mixed: bool, queued: bool) {
    use axial_app::import::{Inventory, ReadOnlySource};

    let temporary =
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let baseline = temporary.path().join("predecessor");
    let originals = benchmark_predecessor_files(&baseline, mixed, queued);
    let original_modified: Vec<_> = originals
        .iter()
        .map(|(path, _)| std::fs::metadata(path).unwrap().modified().unwrap())
        .collect();
    let profile = temporary.path().join("replacement");
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
    let installed = install_terminal(&api, &installation).await;
    assert_eq!(installed["outcome"], "succeeded", "{installed}");
    let source = ReadOnlySource::from_native_selection(
        services.library.admit_application_root().unwrap(),
        &baseline,
    )
    .unwrap();
    services
        .imports
        .admit(Inventory::capture(&source, &BTreeMap::new()).unwrap())
        .unwrap();
    let preview = api.get("/api/v1/import/preview").await;
    assert_eq!(
        preview["instances"][0]["ordinary_import_available"], true,
        "{preview}"
    );
    assert_eq!(preview["cutover_available"], false);
    let import_request = json!({
        "fingerprint":preview["fingerprint"],"legacy_id":"0000000000000001"
    });
    let imported = api
        .post("/api/v1/import/instances", import_request.clone())
        .await;
    assert_eq!(imported["cutover_available"], false);
    let instance = imported["instance"]["id"].as_str().unwrap().to_owned();
    let repeated = api.post("/api/v1/import/instances", import_request).await;
    assert_eq!(repeated["instance"]["id"], instance);
    assert_eq!(repeated["cutover_available"], false);
    wait_launchable(&api, &instance).await;
    assert!(
        services.sessions.snapshots().is_empty(),
        "import cannot launch historical work"
    );

    let drivers = api.get("/api/v1/launch/benchmark/suite/drivers").await;
    assert_eq!(drivers["drivers"].as_array().unwrap().len(), 1);
    let original_driver = drivers["drivers"][0]["driver"].clone();
    if queued {
        assert_eq!(original_driver["state"], "interrupted");
        assert_eq!(
            original_driver["error"],
            "driver automatic resume queued after restart"
        );
    }
    let source_driver = original_driver["id"].as_str().unwrap().to_owned();
    let source_suite = original_driver["suite_id"].as_str().unwrap().to_owned();
    let driver_path = format!("/api/v1/launch/benchmark/suite/drivers/{source_driver}");
    let suite_path = format!("/api/v1/launch/benchmark/suites/{source_suite}");
    let original_suite = api.get(&suite_path).await;
    let original_report = original_suite["runs"][0]["session_id"].as_str();
    let historical_bytes =
        retained_benchmark_bytes(&services, &source_suite, &source_driver, original_report);
    let ready = api.get(&driver_path).await;
    assert_eq!(ready["view_model"]["can_resume"], true, "{ready}");
    assert!(ready.get("resumed_driver_id").is_none());
    for run in original_suite["runs"].as_array().unwrap() {
        assert!(run.get("launch_intent").is_none());
    }

    services.imports.forget().unwrap();
    drop(source);
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    let services = start_profile_with_test_endpoints(profile.clone(), provider.endpoints())
        .await
        .unwrap();
    let api = Api::new(&services);
    assert_eq!(api.get(&driver_path).await, ready);
    assert_eq!(api.get(&suite_path).await, original_suite);
    assert_eq!(services.benchmarks.resume_interrupted_drivers().unwrap(), 0);
    assert!(services.sessions.snapshots().is_empty());
    assert!(services.tasks.status().is_idle());
    services.instances.registry().storage().read(|db| {
        let (intents, drivers, runnable): (i64, i64, i64) = db.query_row(
            "SELECT (SELECT COUNT(*) FROM launch_intents), (SELECT COUNT(*) FROM benchmark_drivers), (SELECT COUNT(*) FROM benchmark_drivers WHERE request IS NOT NULL)",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!((intents, drivers, runnable), (0, 1, 0), "import and reopen must not schedule predecessor work");
        Ok::<_, axial_app::storage::StorageError>(())
    }).unwrap();
    assert_eq!(
        retained_benchmark_bytes(&services, &source_suite, &source_driver, original_report),
        historical_bytes
    );

    // Discard the accepted HTTP response body. The source GET must recover the
    // durable successor without issuing a second command or inventing an ID.
    let response = api
        .client
        .post(format!("{}{driver_path}/resume", api.base))
        .header(transport::CAPABILITY_HEADER, &api.capability)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    drop(response);
    let reconciled = api.get(&driver_path).await;
    assert_eq!(reconciled["driver"], original_driver);
    assert_eq!(reconciled["view_model"]["can_resume"], false);
    let successor_driver = reconciled["resumed_driver_id"].as_str().unwrap().to_owned();
    assert_ne!(successor_driver, source_driver);
    let successor_path = format!("/api/v1/launch/benchmark/suite/drivers/{successor_driver}");
    let accepted = api.get(&successor_path).await;
    assert_ne!(accepted["driver"]["historical"], true);
    let successor_suite = accepted["driver"]["suite_id"].as_str().unwrap().to_owned();
    assert_ne!(successor_suite, source_suite);
    let successor_suite_path = format!("/api/v1/launch/benchmark/suites/{successor_suite}");
    let accepted_suite = api.get(&successor_suite_path).await;
    for (run, original) in accepted_suite["runs"]
        .as_array()
        .unwrap()
        .iter()
        .zip(original_suite["runs"].as_array().unwrap())
    {
        if original["state"] == "pending" {
            assert!(uuid::Uuid::parse_str(run["launch_intent"].as_str().unwrap()).is_ok());
        } else {
            assert_eq!(run, original);
            assert!(run.get("launch_intent").is_none());
        }
    }
    let replay = api.post(&format!("{driver_path}/resume"), json!({})).await;
    assert_eq!(replay["driver"]["id"], successor_driver);
    assert_eq!(replay["driver"]["suite_id"], successor_suite);

    let mut executed = BTreeSet::new();
    for index in usize::from(mixed)..2 {
        let run = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let suite = api.get(&successor_suite_path).await;
                if suite["runs"][index]["state"] == "running" {
                    break suite["runs"][index].clone();
                }
                let driver = api.get(&successor_path).await;
                assert_ne!(driver["driver"]["state"], "failed", "{driver}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("remaining benchmark run must reach a real owned session");
        let session = run["session_id"].as_str().unwrap();
        let intent = run["launch_intent"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(intent).is_ok());
        assert!(
            executed.insert(session.to_owned()),
            "a remaining run must not reuse a session"
        );
        assert_ne!(Some(session), original_report);
        observe_and_stop_session(&api, session).await;
        let accepted_intent = api.get(&format!("/api/v1/launch/intents/{intent}")).await;
        assert_eq!(accepted_intent["state"], "accepted");
        assert_eq!(accepted_intent["session"]["session_id"], session);
        let report = api.get(&format!("/api/v1/launch/reports/{session}")).await;
        assert_eq!(report["instance_id"], instance);
        assert_eq!(report["session_outcome"]["kind"], "stopped");
        assert_eq!(
            report["scenario"]["benchmark_id"],
            original_suite["runs"][index]["benchmark_id"]
        );
        assert_eq!(
            report["scenario"]["benchmark_profile"],
            original_suite["runs"][index]["profile"]
        );
        assert_eq!(
            report["scenario"]["benchmark_run_type"],
            original_suite["runs"][index]["run_type"]
        );
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let driver = api.get(&successor_path).await;
            if driver["driver"]["state"] == "complete" {
                assert!(driver["driver"]["pending_run_index"].is_null(), "{driver}");
                break;
            }
            assert_ne!(driver["driver"]["state"], "failed", "{driver}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("remaining benchmark runs must settle the successor driver");
    let complete_suite = api.get(&successor_suite_path).await;
    if mixed {
        assert_eq!(complete_suite["runs"][0], original_suite["runs"][0]);
        assert!(complete_suite["runs"][0].get("launch_intent").is_none());
    }
    assert_eq!(executed.len(), 2 - usize::from(mixed));
    for session in &executed {
        assert!(services.sessions.snapshot_by_session_id(session).is_some());
    }
    assert_eq!(
        retained_benchmark_bytes(&services, &source_suite, &source_driver, original_report),
        historical_bytes
    );
    provider.assert_requests(true);
    services.imports.forget().unwrap();
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    drop(services);
    provider.shutdown().await;

    let reopened = start_in_profile(profile, None).await.unwrap();
    let api = Api::new(&reopened);
    let restored = api.get(&driver_path).await;
    assert_eq!(restored["driver"], original_driver);
    assert_eq!(restored["resumed_driver_id"], successor_driver);
    assert_eq!(restored["view_model"]["can_resume"], false);
    assert_eq!(api.get(&suite_path).await, original_suite);
    assert_eq!(api.get(&successor_suite_path).await, complete_suite);
    let replay = api.post(&format!("{driver_path}/resume"), json!({})).await;
    assert_eq!(replay["driver"]["id"], successor_driver);
    assert_eq!(replay["driver"]["state"], "complete");
    assert_eq!(
        api.get("/api/v1/launch/benchmark/suite/drivers").await["drivers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let reports = api.get("/api/v1/launch/reports").await;
    let reports = reports["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 2, "one report per inherited or executed run");
    for session in &executed {
        assert_eq!(
            reports
                .iter()
                .filter(|report| report["session_id"] == *session)
                .count(),
            1
        );
    }
    reopened
        .instances
        .registry()
        .storage()
        .read(|db| {
            let count: i64 =
                db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get(0))?;
            assert_eq!(
                count,
                executed.len() as i64,
                "inherited history has no fabricated launch intent"
            );
            let suites: i64 = db.query_row("SELECT count(*) FROM benchmark_suites", [], |row| {
                row.get(0)
            })?;
            assert_eq!(suites, 2, "Resume accepts only one successor suite");
            Ok::<_, axial_app::storage::StorageError>(())
        })
        .unwrap();
    assert!(
        reopened.sessions.snapshots().is_empty(),
        "source replay cannot execute completed runs"
    );
    assert_eq!(
        retained_benchmark_bytes(&reopened, &source_suite, &source_driver, original_report),
        historical_bytes
    );
    for ((path, bytes), modified) in originals.iter().zip(original_modified) {
        assert_eq!(std::fs::read(path).unwrap(), *bytes, "{}", path.display());
        assert_eq!(
            std::fs::metadata(path).unwrap().modified().unwrap(),
            modified,
            "{}",
            path.display()
        );
    }
    reopened.server.shutdown().await.unwrap();
    assert!(reopened.server.is_shutdown_settled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_imported_benchmark_resume_all_pending_executes_once_and_reopens() {
    imported_benchmark_resume_journey(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_imported_benchmark_resume_mixed_preserves_terminal_and_executes_remaining() {
    imported_benchmark_resume_journey(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_imported_benchmark_resume_queued_all_pending_requires_explicit_command() {
    imported_benchmark_resume_journey(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_imported_benchmark_resume_queued_mixed_preserves_source_and_executes_remaining() {
    imported_benchmark_resume_journey(true, true).await;
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
