use super::super::{
    logs::{LogCollector, LogStream},
    process::{OwnedProcess, SpawnError},
};
use super::*;
use crate::{
    install::artifacts::{ActivatedFile, ActivatedVersion},
    instances::{
        create::{CreateInstanceRequest, CreateTarget, InstanceService},
        directory::{InstanceDirectories, Registry},
    },
    library::{LibraryLifecycle, LibraryOpenOutcome},
    storage::MetadataStore,
    tasks::{Exclusions, TaskOwner},
};
use sha1::{Digest, Sha1};
use std::{
    io::{Cursor, Write},
    path::{Component, Path, PathBuf, Prefix},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;

const MAIN_CLASS: &[u8] = &[
    202, 254, 186, 190, 0, 0, 0, 52, 0, 8, 1, 0, 30, 110, 101, 116, 47, 109, 105, 110, 101, 99,
    114, 97, 102, 116, 47, 99, 108, 105, 101, 110, 116, 47, 109, 97, 105, 110, 47, 77, 97, 105,
    110, 7, 0, 1, 1, 0, 16, 106, 97, 118, 97, 47, 108, 97, 110, 103, 47, 79, 98, 106, 101, 99, 116,
    7, 0, 3, 1, 0, 4, 109, 97, 105, 110, 1, 0, 22, 40, 91, 76, 106, 97, 118, 97, 47, 108, 97, 110,
    103, 47, 83, 116, 114, 105, 110, 103, 59, 41, 86, 1, 0, 4, 67, 111, 100, 101, 0, 33, 0, 2, 0,
    4, 0, 0, 0, 0, 0, 1, 0, 9, 0, 5, 0, 6, 0, 1, 0, 7, 0, 0, 0, 13, 0, 0, 0, 1, 0, 0, 0, 1, 177, 0,
    0, 0, 0, 0, 0,
];

fn ordinary_fixture_path(path: &Path) -> PathBuf {
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        panic!("canonical local Windows fixture prefix");
    };
    let drive = match prefix.kind() {
        Prefix::VerbatimDisk(drive) | Prefix::Disk(drive) => drive,
        _ => panic!("canonical local Windows fixture drive"),
    };
    assert_eq!(parts.next(), Some(Component::RootDir));
    let mut result = PathBuf::from(format!("{}:\\", char::from(drive)));
    for part in parts {
        let Component::Normal(name) = part else {
            panic!("canonical fixture component");
        };
        result.push(name);
    }
    result
}

async fn drain(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    stream: LogStream,
    collector: Arc<Mutex<LogCollector>>,
) -> bool {
    let mut buffer = Box::new([0; 8192]);
    loop {
        match pipe.read(buffer.as_mut()).await {
            Ok(0) => {
                collector.lock().unwrap().finish_stream(stream);
                return true;
            }
            Ok(count) => {
                collector.lock().unwrap().push(stream, &buffer[..count]);
            }
            Err(_) => return false,
        }
    }
}

async fn execute(
    command: &ValidatedLaunchCommand,
    args: &[String],
) -> (Option<i32>, bool, Vec<super::super::outcome::FailureClass>) {
    command.revalidate().unwrap();
    let mut native = tokio::process::Command::new(command.program());
    native
        .args(args)
        .current_dir(command.cwd())
        .envs(command.env());
    for name in [
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
    ] {
        native.env_remove(name);
    }
    let (mut process, output, spawn_failed) = match OwnedProcess::spawn(native) {
        Ok((process, output)) => (process, output, false),
        Err(SpawnError::Unsettled(process, output, _)) => (process, output, true),
        Err(SpawnError::BeforeSpawn(_)) => {
            panic!("real Java fixture refused before process creation")
        }
    };
    let collector = Arc::new(Mutex::new(LogCollector::new(vec![])));
    let stdout = tokio::spawn(drain(output.stdout, LogStream::Stdout, collector.clone()));
    let stderr = tokio::spawn(drain(output.stderr, LogStream::Stderr, collector.clone()));
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stopped = spawn_failed;
    loop {
        if process.tree_settled().unwrap_or(false) {
            break;
        }
        if stopped || Instant::now() >= deadline {
            stopped = true;
            let _ = process.terminate();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let status = process
        .try_wait()
        .ok()
        .flatten()
        .and_then(|status| status.code());
    let stdout_drained = stdout.await.unwrap_or(false);
    let stderr_drained = stderr.await.unwrap_or(false);
    let collector = collector.lock().unwrap();
    (
        status,
        !stopped && stdout_drained && stderr_drained && collector.outputs_drained(),
        collector.evidence().failure_classes.clone(),
    )
}

#[tokio::test]
#[ignore = "requires AXIAL_TEST_WINDOWS_RUNTIME_ROOT pointing to frozen official Java 17"]
async fn real_java_loads_verified_classpath_from_canonical_windows_capabilities() {
    let parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let root = tempfile::Builder::new()
        .prefix("Java classpath café ")
        .tempdir_in(&parent)
        .unwrap();
    let canonical_root = std::fs::canonicalize(root.path()).unwrap();
    let library = match LibraryLifecycle::open(&canonical_root) {
        LibraryOpenOutcome::Ready(library) => library,
        other => panic!("fixture library unavailable: {other:?}"),
    };
    let storage = Arc::new(MetadataStore::in_memory().unwrap());
    storage
        .migrate(&[
            crate::instances::directory::MIGRATION,
            crate::instances::create::MIGRATION,
            crate::content::install::MIGRATION,
            crate::performance::mutation::MIGRATION,
        ])
        .unwrap();
    let tasks = TaskOwner::new(1).unwrap();
    let directories =
        InstanceDirectories::new(Registry::new(storage), library.clone(), Exclusions::new());
    let instances = InstanceService::new(directories.clone(), tasks.clone());
    let created = instances
        .create(
            CreateInstanceRequest {
                name: "Java classpath proof".into(),
                selection_id: "1.20.1".into(),
                ..Default::default()
            },
            CreateTarget {
                selection_id: "1.20.1".into(),
                version_id: "1.20.1".into(),
                minecraft_version: "1.20.1".into(),
                loader_key: "vanilla".into(),
            },
            instances.creation_admission_for_tests().await.unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let instance = directories.admit(&created.id).unwrap();
    let mut jar = zip::ZipWriter::new(Cursor::new(Vec::new()));
    jar.start_file(
        "net/minecraft/client/main/Main.class",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    jar.write_all(MAIN_CLASS).unwrap();
    let jar = jar.finish().unwrap().into_inner();
    let metadata = br#"{"id":"1.20.1","type":"release","mainClass":"net.minecraft.client.main.Main","javaVersion":{"component":"java-runtime-gamma","majorVersion":17},"arguments":{"jvm":["-cp","${classpath}"],"game":[]}}"#;
    let mut files = Vec::new();
    for (path, bytes) in [
        ("versions/1.20.1/1.20.1.json", metadata.as_slice()),
        ("versions/1.20.1/1.20.1.jar", jar.as_slice()),
    ] {
        let target = canonical_root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, bytes).unwrap();
        files.push(ActivatedFile {
            path: path.into(),
            sha1: hex::encode(Sha1::digest(bytes)),
            size: bytes.len() as u64,
        });
    }
    let installed = ActivatedVersion {
        version_id: "1.20.1".into(),
        contract_id: format!("managed-install-activation-v1.{}", "A".repeat(43)),
        files,
    }
    .verify(instance.generation().clone())
    .unwrap();
    let source = std::fs::canonicalize(
        std::env::var_os("AXIAL_TEST_WINDOWS_RUNTIME_ROOT")
            .expect("frozen official Java 17 runtime root"),
    )
    .unwrap();
    let runtime = crate::runtime::probe::probe_java_runtime(&source.join("bin/java.exe"), None)
        .await
        .unwrap();
    let operation = instance.generation().managed_library().unwrap();
    let command = super::build(super::super::model::LaunchPlanRequest {
        version_guard: axial_minecraft::VersionBundleReadGuard::acquire(&operation).unwrap(),
        library_operation: operation,
        library_dir: instance.generation().read_projection().unwrap(),
        target_version_id: "1.20.1".into(),
        game_dir: instance.game_directory().read_projection().unwrap(),
        auth: super::super::model::LaunchAuthContext::offline("Player"),
        runtime,
        managed_launch: None,
        settings: LaunchOptions {
            min_memory_mb: Some(256),
            max_memory_mb: Some(512),
            ..Default::default()
        },
        installed,
        prepared_natives: None,
    })
    .unwrap();
    let classpath_index = command
        .args()
        .iter()
        .position(|value| value == "-cp")
        .unwrap()
        + 1;
    let mut control_args = command.args().to_vec();
    control_args[classpath_index] = ordinary_fixture_path(command.installed.client_jar())
        .to_str()
        .unwrap()
        .into();
    let control = execute(&command, &control_args).await;
    let production = execute(&command, command.args()).await;
    let shutdown = tasks.shutdown(Duration::from_secs(5)).await;
    let prefix_kind = match command.installed.client_jar().components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) => "disk",
            Prefix::UNC(_, _) => "unc",
            Prefix::VerbatimDisk(_) => "verbatim_disk",
            Prefix::VerbatimUNC(_, _) => "verbatim_unc",
            _ => "unsupported",
        },
        _ => "missing",
    };
    eprintln!(
        "{}",
        serde_json::json!({
            "fixture": "verified-classpath-native-java17",
            "retained_path_prefix_kind": prefix_kind,
            "control": {"exit_code": control.0, "owner_settled_and_drained": control.1, "failure_classes": control.2},
            "production": {"exit_code": production.0, "owner_settled_and_drained": production.1, "failure_classes": production.2},
        })
    );
    shutdown.unwrap();
    assert_eq!(
        control.0,
        Some(0),
        "ordinary Win32 path control must load the verified class"
    );
    assert!(control.1);
    assert_eq!(
        production.0,
        Some(0),
        "production classpath must reach the real Java entrypoint"
    );
    assert!(production.1);
}
