use crate::find_java_runtime;
use crate::java::JavaRuntimeLookupError;
use crate::launch::{JavaVersion, Library, maven_to_path};
use crate::paths::{libraries_dir, versions_dir};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tokio::process::Command;
use zip::ZipArchive;

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

#[derive(Debug, Deserialize)]
struct DataEntry {
    #[serde(default)]
    client: String,
}

pub async fn run_processors<F>(
    mc_dir: &Path,
    mc_version: &str,
    install_profile_json: &[u8],
    installer_data: &[u8],
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

    let (data_vars, temp_dir) = build_data_vars(&profile.data, mc_dir, mc_version, installer_data)?;
    let processors = profile
        .processors
        .into_iter()
        .filter(|processor| {
            processor.sides.is_empty() || processor.sides.iter().any(|side| side == "client")
        })
        .collect::<Vec<_>>();

    let total = processors.len();
    for (index, processor) in processors.iter().enumerate() {
        progress(
            index + 1,
            total,
            format!("Processor {}/{}", index + 1, total),
        );
        run_processor(&java_path, processor, &lib_paths, &data_vars, &lib_dir).await?;
    }

    if let Some(temp_dir) = temp_dir {
        let _ = fs::remove_dir_all(temp_dir);
    }
    Ok(())
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
) -> Result<(HashMap<String, String>, Option<PathBuf>), ProcessorError> {
    let mut vars = HashMap::new();
    let mut temp_dir = None;

    for (key, entry) in data {
        let value = entry.client.trim();
        if value.is_empty() {
            continue;
        }

        if value.starts_with('[') && value.ends_with(']') {
            let coord = &value[1..value.len() - 1];
            let maven_path = maven_to_path(coord);
            if !maven_path.as_os_str().is_empty() {
                vars.insert(
                    key.clone(),
                    libraries_dir(mc_dir)
                        .join(maven_path)
                        .to_string_lossy()
                        .to_string(),
                );
            }
            continue;
        }

        if let Some(entry_path) = value.strip_prefix('/') {
            if temp_dir.is_none() {
                temp_dir = Some(create_temp_dir()?);
            }
            let extracted = extract_from_installer_jar(
                installer_data,
                entry_path,
                temp_dir.as_ref().expect("temp dir just set"),
            )?;
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

    Ok((vars, temp_dir))
}

fn create_temp_dir() -> Result<PathBuf, std::io::Error> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("croopor-forge-processors-{nanos:x}"));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn extract_from_installer_jar(
    jar_data: &[u8],
    entry_path: &str,
    temp_dir: &Path,
) -> Result<PathBuf, ProcessorError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(jar_data))?;
    let mut file = archive
        .by_name(entry_path)
        .map_err(|_| ProcessorError::Command(format!("extracting {entry_path} from installer")))?;
    let destination = temp_dir.join(Path::new(entry_path));
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = fs::File::create(&destination)?;
    std::io::copy(&mut file, &mut output)?;
    Ok(destination)
}

async fn run_processor(
    java_path: &Path,
    processor: &Processor,
    lib_paths: &HashMap<String, PathBuf>,
    data_vars: &HashMap<String, String>,
    lib_dir: &Path,
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
    let main_class = read_main_class_from_jar(&proc_jar_path)?;

    let mut args = vec!["-cp".to_string(), classpath, main_class];
    for arg in &processor.args {
        args.push(substitute_arg(arg, lib_paths, data_vars, lib_dir, 0));
    }

    let mut command = Command::new(java_path);
    command.args(args);
    command.current_dir(lib_dir);
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
