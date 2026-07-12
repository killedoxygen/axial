use crate::artifact_path::ArtifactRelativePath;
use crate::launch::{JavaVersion, Library, maven_to_path};
use crate::loaders::workspace::cleanup::LoaderWorkspace;
use crate::paths::{libraries_dir, versions_dir};
use crate::{JavaRuntimeLookupError, find_java_runtime};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;
use zip::ZipArchive;

#[cfg(not(test))]
const MAX_INSTALLER_DATA_ENTRY_BYTES: u64 = 128 << 20;
#[cfg(test)]
const MAX_INSTALLER_DATA_ENTRY_BYTES: u64 = 1024;
const PROCESSOR_DATA_DIR: &str = "processor-data";

#[derive(Debug, Error)]
pub enum ProcessorError {
    #[error("invalid install profile json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip failed: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("processor runtime not found: {0}")]
    Java(#[from] JavaRuntimeLookupError),
    #[error("{0}")]
    Command(String),
}

#[derive(Debug, Deserialize)]
struct InstallProfile {
    #[serde(default)]
    processors: Vec<Processor>,
    #[serde(default)]
    libraries: Vec<Library>,
    #[serde(default)]
    data: HashMap<String, DataEntry>,
}

#[derive(Debug, Deserialize)]
struct Processor {
    jar: String,
    #[serde(default)]
    classpath: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    sides: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct DataEntry {
    #[serde(default)]
    client: String,
}

pub async fn run_processors<F>(
    mc_dir: &Path,
    mc_version: &str,
    install_profile_json: &[u8],
    installer_data: &[u8],
    workspace: &LoaderWorkspace,
    mut progress: F,
) -> Result<(), ProcessorError>
where
    F: FnMut(usize, usize, String),
{
    let profile = serde_json::from_slice::<InstallProfile>(install_profile_json)?;
    if profile.processors.is_empty() {
        return Ok(());
    }

    let java_path = find_java_for_processors(mc_dir)?;
    let lib_dir = libraries_dir(mc_dir);
    let mut lib_paths = HashMap::new();
    for lib in &profile.libraries {
        let maven_path = maven_to_path(&lib.name);
        if !maven_path.as_os_str().is_empty() {
            lib_paths.insert(lib.name.clone(), lib_dir.join(maven_path));
        }
    }

    let installer_path = workspace.path().join("source-installer.jar");
    let (data_vars, data_plan) = build_data_vars_blocking(
        &profile.data,
        mc_dir,
        mc_version,
        installer_data,
        workspace.path(),
        &installer_path,
    )
    .await?;
    let data_dir = if let Some(plan) = data_plan {
        let directory = workspace
            .create_temp(PROCESSOR_DATA_DIR)
            .map_err(|error| ProcessorError::Io(loader_io_error(error)))?;
        for file in plan.files {
            if let Err(error) = directory
                .write_relative_exact(&file.relative_path, &file.bytes)
                .await
            {
                let cleanup = directory.cleanup().map_err(loader_io_error);
                cleanup?;
                return Err(ProcessorError::Io(loader_io_error(error)));
            }
        }
        Some(directory)
    } else {
        None
    };
    let processors = profile
        .processors
        .into_iter()
        .filter(|processor| {
            processor.sides.is_empty() || processor.sides.iter().any(|side| side == "client")
        })
        .collect::<Vec<_>>();

    let result = async {
        let total = processors.len();
        for (index, processor) in processors.iter().enumerate() {
            progress(
                index + 1,
                total,
                format!("Processor {}/{}", index + 1, total),
            );
            run_processor(
                &java_path, processor, &lib_paths, &data_vars, &lib_dir, &lib_dir,
            )
            .await?;
        }
        Ok::<(), ProcessorError>(())
    }
    .await;
    let cleanup = data_dir
        .map(|directory| directory.cleanup().map_err(loader_io_error))
        .transpose();
    result?;
    cleanup.map_err(ProcessorError::Io)?;
    Ok(())
}

fn loader_io_error(error: crate::loaders::types::LoaderError) -> io::Error {
    match error {
        crate::loaders::types::LoaderError::Io(error) => error,
        error => io::Error::other(error.to_string()),
    }
}

fn find_java_for_processors(mc_dir: &Path) -> Result<PathBuf, ProcessorError> {
    let components = [
        "java-runtime-delta",
        "java-runtime-gamma",
        "java-runtime-beta",
        "java-runtime-alpha",
        "jre-legacy",
    ];
    let majors = [21, 17, 11, 8];
    for major in majors {
        for component in components {
            if let Ok(runtime) = find_java_runtime(
                mc_dir,
                &JavaVersion {
                    component: component.to_string(),
                    major_version: major,
                },
                "",
            ) {
                return Ok(normalize_java_path(PathBuf::from(runtime.path)));
            }
        }
    }

    find_java_on_path().map(normalize_java_path).ok_or_else(|| {
        ProcessorError::Command(
            "no Java runtime found; install the base game version first to download Java"
                .to_string(),
        )
    })
}

fn find_java_on_path() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        OsStr::new("java.exe")
    } else {
        OsStr::new("java")
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn normalize_java_path(path: PathBuf) -> PathBuf {
    if !cfg!(target_os = "windows") {
        return path;
    }
    if path
        .file_name()
        .map(|name| name.to_string_lossy().eq_ignore_ascii_case("javaw.exe"))
        .unwrap_or(false)
    {
        let candidate = path.with_file_name("java.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    path
}

fn build_data_vars(
    data: &HashMap<String, DataEntry>,
    mc_dir: &Path,
    mc_version: &str,
    installer_data: &[u8],
    work_dir: &Path,
    installer_path: &Path,
) -> Result<(HashMap<String, String>, Option<ProcessorDataPlan>), ProcessorError> {
    let mut vars = HashMap::new();
    let mut data_plan = None;

    for (key, entry) in data {
        let value = entry.client.trim();
        if value.is_empty() {
            continue;
        }

        if value.starts_with('[') && value.ends_with(']') {
            let coord = &value[1..value.len() - 1];
            let maven_path = maven_to_path(coord);
            if !maven_path.as_os_str().is_empty() {
                let path = libraries_dir(mc_dir).join(maven_path);
                vars.insert(key.clone(), path.to_string_lossy().to_string());
            }
            continue;
        }

        if let Some(entry_path) = value.strip_prefix('/') {
            let destination_path = safe_installer_entry_path(entry_path)?;
            let relative_path = ArtifactRelativePath::from_path(&destination_path)
                .map_err(|_| unsafe_installer_entry_error(entry_path))?;
            let plan = data_plan.get_or_insert_with(|| ProcessorDataPlan { files: Vec::new() });
            let extracted = work_dir.join(PROCESSOR_DATA_DIR).join(&destination_path);
            let bytes = read_installer_entry(installer_data, entry_path)?;
            plan.files.push(PlannedWorkspaceFile {
                relative_path,
                bytes,
            });
            vars.insert(key.clone(), extracted.to_string_lossy().to_string());
            continue;
        }

        vars.insert(key.clone(), value.to_string());
    }

    vars.insert(
        "MINECRAFT_JAR".to_string(),
        versions_dir(mc_dir)
            .join(mc_version)
            .join(format!("{mc_version}.jar"))
            .to_string_lossy()
            .to_string(),
    );
    vars.insert("SIDE".to_string(), "client".to_string());
    vars.insert("MINECRAFT_VERSION".to_string(), mc_version.to_string());
    vars.insert("ROOT".to_string(), mc_dir.to_string_lossy().to_string());
    vars.insert(
        "LIBRARY_DIR".to_string(),
        libraries_dir(mc_dir).to_string_lossy().to_string(),
    );
    vars.insert(
        "INSTALLER".to_string(),
        installer_path.to_string_lossy().to_string(),
    );

    Ok((vars, data_plan))
}

async fn build_data_vars_blocking(
    data: &HashMap<String, DataEntry>,
    mc_dir: &Path,
    mc_version: &str,
    installer_data: &[u8],
    work_dir: &Path,
    installer_path: &Path,
) -> Result<(HashMap<String, String>, Option<ProcessorDataPlan>), ProcessorError> {
    let data = data.clone();
    let mc_dir = mc_dir.to_path_buf();
    let mc_version = mc_version.to_string();
    let installer_data = installer_data.to_vec();
    let work_dir = work_dir.to_path_buf();
    let installer_path = installer_path.to_path_buf();

    tokio::task::spawn_blocking(move || {
        build_data_vars(
            &data,
            &mc_dir,
            &mc_version,
            &installer_data,
            &work_dir,
            &installer_path,
        )
    })
    .await
    .map_err(|error| ProcessorError::Command(format!("blocking task failed: {error}")))?
}

#[derive(Debug)]
struct ProcessorDataPlan {
    files: Vec<PlannedWorkspaceFile>,
}

#[derive(Debug)]
struct PlannedWorkspaceFile {
    relative_path: ArtifactRelativePath,
    bytes: Vec<u8>,
}

fn read_installer_entry(jar_data: &[u8], entry_path: &str) -> Result<Vec<u8>, ProcessorError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(jar_data))?;
    let mut file = archive
        .by_name(entry_path)
        .map_err(|_| ProcessorError::Command(format!("extracting {entry_path} from installer")))?;
    let declared_size = file.size();
    if declared_size > MAX_INSTALLER_DATA_ENTRY_BYTES {
        return Err(oversized_installer_entry_error(entry_path));
    }
    let capacity =
        usize::try_from(declared_size).map_err(|_| oversized_installer_entry_error(entry_path))?;
    let mut output = Vec::with_capacity(capacity);
    let mut bounded = (&mut file).take(MAX_INSTALLER_DATA_ENTRY_BYTES + 1);
    bounded.read_to_end(&mut output)?;
    if output.len() as u64 > MAX_INSTALLER_DATA_ENTRY_BYTES || output.len() as u64 != declared_size
    {
        return Err(oversized_installer_entry_error(entry_path));
    }
    Ok(output)
}

fn safe_installer_entry_path(entry_path: &str) -> Result<PathBuf, ProcessorError> {
    let path = Path::new(entry_path);
    if entry_path.trim().is_empty() {
        return Err(unsafe_installer_entry_error(entry_path));
    }

    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => safe.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_installer_entry_error(entry_path));
            }
        }
    }

    if safe.as_os_str().is_empty() {
        return Err(unsafe_installer_entry_error(entry_path));
    }

    Ok(safe)
}

fn unsafe_installer_entry_error(entry_path: &str) -> ProcessorError {
    ProcessorError::Command(format!("unsafe installer entry path: {entry_path}"))
}

fn oversized_installer_entry_error(entry_path: &str) -> ProcessorError {
    ProcessorError::Command(format!("installer entry too large: {entry_path}"))
}

async fn run_processor(
    java_path: &Path,
    processor: &Processor,
    lib_paths: &HashMap<String, PathBuf>,
    data_vars: &HashMap<String, String>,
    lib_dir: &Path,
    root_dir: &Path,
) -> Result<(), ProcessorError> {
    let mut classpath_parts = Vec::new();
    let proc_jar_path =
        resolve_coordinate_path(&processor.jar, lib_paths, lib_dir).ok_or_else(|| {
            ProcessorError::Command(format!("cannot resolve processor jar: {}", processor.jar))
        })?;
    classpath_parts.push(proc_jar_path.clone());

    for cp in &processor.classpath {
        if let Some(path) = resolve_coordinate_path(cp, lib_paths, lib_dir) {
            classpath_parts.push(path);
        }
    }

    let separator = if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    };
    let classpath = classpath_parts
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(separator);
    let main_class = read_main_class_from_jar_blocking(proc_jar_path.clone()).await?;

    let mut args = vec!["-cp".to_string(), classpath, main_class];
    for arg in &processor.args {
        args.push(substitute_arg(arg, lib_paths, data_vars, lib_dir, 0));
    }

    let mut command = Command::new(java_path);
    command.args(args);
    command.current_dir(root_dir);
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .map_err(|_| ProcessorError::Command("processor timed out after 120s".to_string()))?
        .map_err(ProcessorError::Io)?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ProcessorError::Command(if detail.is_empty() {
            format!("processor exited with status {}", output.status)
        } else {
            format!("{}\noutput: {}", output.status, detail)
        }));
    }

    Ok(())
}

fn resolve_coordinate_path(
    coordinate: &str,
    lib_paths: &HashMap<String, PathBuf>,
    lib_dir: &Path,
) -> Option<PathBuf> {
    if let Some(path) = lib_paths.get(coordinate) {
        return Some(path.clone());
    }
    let maven_path = maven_to_path(coordinate);
    if maven_path.as_os_str().is_empty() {
        None
    } else {
        Some(lib_dir.join(maven_path))
    }
}

fn read_main_class_from_jar(jar_path: &Path) -> Result<String, ProcessorError> {
    let file = fs::File::open(jar_path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut manifest = archive.by_name("META-INF/MANIFEST.MF")?;
    let mut data = Vec::new();
    manifest.read_to_end(&mut data)?;
    parse_manifest_main_class(&data)
        .ok_or_else(|| ProcessorError::Command("no Main-Class in manifest".to_string()))
}

async fn read_main_class_from_jar_blocking(jar_path: PathBuf) -> Result<String, ProcessorError> {
    tokio::task::spawn_blocking(move || read_main_class_from_jar(&jar_path))
        .await
        .map_err(|error| ProcessorError::Command(format!("blocking task failed: {error}")))?
}

fn parse_manifest_main_class(data: &[u8]) -> Option<String> {
    let mut current = String::new();
    let content = String::from_utf8_lossy(data).replace("\r\n", "\n");
    for line in content.lines() {
        if line.is_empty() {
            if let Some(value) = current.strip_prefix("Main-Class:") {
                return Some(value.trim().to_string());
            }
            current.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            current.push_str(rest);
            continue;
        }
        if let Some(value) = current.strip_prefix("Main-Class:") {
            return Some(value.trim().to_string());
        }
        current = line.to_string();
    }
    current
        .strip_prefix("Main-Class:")
        .map(|value| value.trim().to_string())
}

fn substitute_arg(
    arg: &str,
    lib_paths: &HashMap<String, PathBuf>,
    data_vars: &HashMap<String, String>,
    lib_dir: &Path,
    depth: usize,
) -> String {
    if depth > 8 || arg.is_empty() {
        return arg.to_string();
    }

    let mut replaced = arg.to_string();
    while let (Some(start), Some(end)) = (replaced.find('['), replaced.find(']')) {
        if end <= start {
            break;
        }
        let coord = &replaced[start + 1..end];
        let Some(path) = resolve_coordinate_path(coord, lib_paths, lib_dir) else {
            break;
        };
        replaced = format!(
            "{}{}{}",
            &replaced[..start],
            path.to_string_lossy(),
            &replaced[end + 1..]
        );
    }

    while let (Some(start), Some(end)) = (replaced.find('{'), replaced.find('}')) {
        if end <= start {
            break;
        }
        let key = &replaced[start + 1..end];
        let Some(value) = data_vars.get(key) else {
            break;
        };
        replaced = format!("{}{}{}", &replaced[..start], value, &replaced[end + 1..]);
    }

    if replaced == arg {
        arg.to_string()
    } else {
        substitute_arg(&replaced, lib_paths, data_vars, lib_dir, depth + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    #[test]
    fn build_data_vars_rejects_unsafe_installer_entry_path() {
        let root = test_root("unsafe-installer-entry");
        let mut data = HashMap::new();
        data.insert(
            "EXTRACTED".to_string(),
            DataEntry {
                client: "/../outside.txt".to_string(),
            },
        );

        let error = build_data_vars(
            &data,
            &root,
            "1.20.1",
            &empty_zip(),
            &root,
            &root.join("installer.jar"),
        )
        .expect_err("unsafe installer entry path should fail");

        assert!(
            matches!(error, ProcessorError::Command(message) if message.contains("unsafe installer entry path"))
        );
        assert!(!root.join("outside.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_data_vars_reports_missing_installer_entry_without_panic() {
        let root = test_root("missing-installer-entry");
        let mut data = HashMap::new();
        data.insert(
            "EXTRACTED".to_string(),
            DataEntry {
                client: "/missing.txt".to_string(),
            },
        );

        let error = build_data_vars(
            &data,
            &root,
            "1.20.1",
            &empty_zip(),
            &root,
            &root.join("installer.jar"),
        )
        .expect_err("missing installer entry should fail");

        assert!(
            matches!(error, ProcessorError::Command(message) if message.contains("extracting missing.txt from installer"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_data_vars_rejects_oversized_installer_entry() {
        let root = test_root("oversized-installer-entry");
        let mut data = HashMap::new();
        data.insert(
            "EXTRACTED".to_string(),
            DataEntry {
                client: "/large.bin".to_string(),
            },
        );
        let installer = zip_with_entry(
            "large.bin",
            vec![b'x'; (MAX_INSTALLER_DATA_ENTRY_BYTES + 1) as usize],
        );

        let error = build_data_vars(
            &data,
            &root,
            "1.20.1",
            &installer,
            &root,
            &root.join("installer.jar"),
        )
        .expect_err("oversized installer entry should fail");

        assert!(
            matches!(error, ProcessorError::Command(message) if message.contains("installer entry too large"))
        );
        assert!(!root.join("large.bin").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_data_vars_reuses_canonical_processor_data_child() {
        let root = test_root("canonical-processor-data");
        let mut data = HashMap::new();
        data.insert(
            "EXTRACTED".to_string(),
            DataEntry {
                client: "/data/input.bin".to_string(),
            },
        );
        let installer = zip_with_entry("data/input.bin", b"input".to_vec());

        for _ in 0..2 {
            let (vars, plan) = build_data_vars(
                &data,
                &root,
                "1.20.1",
                &installer,
                &root,
                &root.join("installer.jar"),
            )
            .expect("processor data plan");

            let expected = root
                .join(PROCESSOR_DATA_DIR)
                .join("data/input.bin")
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                vars.get("EXTRACTED").map(String::as_str),
                Some(expected.as_str())
            );
            assert_eq!(plan.expect("extraction plan").files.len(), 1);
        }

        let _ = fs::remove_dir_all(root);
    }

    fn empty_zip() -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        zip::ZipWriter::new(&mut cursor)
            .finish()
            .expect("finish empty zip");
        cursor.into_inner()
    }

    fn zip_with_entry(name: &str, bytes: Vec<u8>) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            writer
                .start_file(name, SimpleFileOptions::default())
                .expect("start zip file");
            writer.write_all(&bytes).expect("write zip file");
            writer.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "axial-processors-{name}-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test root");
        root
    }
}
