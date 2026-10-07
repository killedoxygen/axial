//! Bounded Java diagnostics with immutable executable evidence.
//!
//! Call from a retained TaskOwner task: cancellation terminates the owned tree
//! and keeps that task alive until process reaping and output drainage settle.

use super::model::{JavaArchitecture, JavaDiscoveryError, JavaRuntimeInfo};
use crate::launch::process::{OwnedProcess, SpawnError};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const MAX_EXECUTABLE_BYTES: u64 = 64 << 20;
const MAX_OUTPUT_BYTES: usize = 64 << 10;
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    canonical: PathBuf,
    size: u64,
    modified: SystemTime,
    sha256: [u8; 32],
    #[cfg(unix)]
    filesystem_identity: (u64, u64, i64, i64, u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutableIdentity {
    requested: PathBuf,
    selected: PathBuf,
    requested_file: FileIdentity,
    selected_file: Option<FileIdentity>,
}

/// Nonforgeable evidence of a successful probe of these exact executable bytes.
/// It is independent of managed-file write authority and never grants any.
#[derive(Clone, Debug)]
pub struct JavaProbeReceipt {
    identity: ExecutableIdentity,
    info: JavaRuntimeInfo,
    architecture: JavaArchitecture,
}

impl JavaProbeReceipt {
    pub fn info(&self) -> &JavaRuntimeInfo {
        &self.info
    }
    pub fn architecture(&self) -> JavaArchitecture {
        self.architecture
    }
    pub fn executable(&self) -> &Path {
        &self.identity.selected
    }

    /// Recheck the requested alias, the selected console sibling, metadata and
    /// digest immediately before spawn. This never starts a process.
    pub fn revalidate(&self) -> Result<(), JavaDiscoveryError> {
        if fingerprint(&self.identity.requested).map_err(|_| JavaDiscoveryError::Replaced)?
            != self.identity
        {
            return Err(JavaDiscoveryError::Replaced);
        }
        Ok(())
    }

    pub fn revalidate_cli_executable(&self) -> Result<PathBuf, JavaDiscoveryError> {
        self.revalidate()?;
        Ok(self.identity.selected.clone())
    }
}

pub async fn probe_java_runtime(
    path: &Path,
    id_hint: Option<&str>,
) -> Result<JavaProbeReceipt, JavaDiscoveryError> {
    probe_java_runtime_until(path, id_hint, std::future::pending()).await
}

pub async fn probe_java_runtime_until(
    path: &Path,
    id_hint: Option<&str>,
    cancellation: impl Future<Output = ()>,
) -> Result<JavaProbeReceipt, JavaDiscoveryError> {
    probe_with_timeout(path, id_hint, cancellation, PROBE_TIMEOUT).await
}

async fn probe_with_timeout(
    path: &Path,
    id_hint: Option<&str>,
    cancellation: impl Future<Output = ()>,
    timeout: Duration,
) -> Result<JavaProbeReceipt, JavaDiscoveryError> {
    let identity = fingerprint(path)?;
    let output = run_probe(&identity.selected, cancellation, timeout).await?;
    if fingerprint(path).map_err(|_| JavaDiscoveryError::Replaced)? != identity {
        return Err(JavaDiscoveryError::Replaced);
    }
    let (major, update) = parse_java_version(&output).ok_or(JavaDiscoveryError::InvalidVersion)?;
    let architecture = property(&output, "os.arch")
        .map(JavaArchitecture::parse)
        .unwrap_or(JavaArchitecture::Unknown);
    if architecture == JavaArchitecture::Unknown {
        return Err(JavaDiscoveryError::UnknownArchitecture);
    }
    Ok(JavaProbeReceipt {
        info: JavaRuntimeInfo {
            id: id_hint.unwrap_or_default().to_owned(),
            major,
            update,
            distribution: detect_distribution(&output).into(),
            path: identity.selected.to_string_lossy().into_owned(),
        },
        identity,
        architecture,
    })
}

async fn run_probe(
    path: &Path,
    cancellation: impl Future<Output = ()>,
    timeout: Duration,
) -> Result<String, JavaDiscoveryError> {
    let mut command = Command::new(path);
    command.args(["-XshowSettings:properties", "-version"]);
    // The selected executable is explicit. Launcher-injected JVM options must
    // not execute agents or alter the meaning of this diagnostic invocation.
    for key in [
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
    ] {
        command.env_remove(key);
    }
    let (mut process, mut output, mut failure) = match OwnedProcess::spawn(command) {
        Ok((process, output)) => (process, output, None),
        Err(SpawnError::BeforeSpawn(error)) => return Err(spawn_error(&error)),
        Err(SpawnError::Unsettled(process, output, _)) => {
            (process, output, Some(JavaDiscoveryError::Failed))
        }
    };
    let deadline = tokio::time::sleep(timeout);
    #[cfg(test)]
    let started = std::time::Instant::now();
    #[cfg(test)]
    let mut last_observation = started;
    #[cfg(test)]
    let mut largest_gap = Duration::ZERO;
    #[cfg(test)]
    let mut iterations = 0_u64;
    #[cfg(test)]
    let mut last_leader = "unobserved";
    #[cfg(test)]
    let mut last_tree = "unobserved";
    tokio::pin!(deadline);
    tokio::pin!(cancellation);
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stdout = [0_u8; 4096];
    let mut stderr = [0_u8; 4096];
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    loop {
        #[cfg(test)]
        {
            let now = std::time::Instant::now();
            largest_gap = largest_gap.max(now.duration_since(last_observation));
            last_observation = now;
            iterations = iterations.saturating_add(1);
        }
        tokio::select! {
            biased;
            _ = &mut cancellation, if failure.is_none() => failure = Some(JavaDiscoveryError::Cancelled),
            _ = &mut deadline, if failure.is_none() => {
                failure = Some(JavaDiscoveryError::TimedOut);
                #[cfg(test)]
                {
                    let now = std::time::Instant::now();
                    let gap = largest_gap.max(now.duration_since(last_observation));
                    let leader_pid = process.pid();
                    let leader_before_termination = match process.try_wait() {
                        Ok(Some(status)) if status.success() => "success",
                        Ok(Some(_)) => "failure",
                        Ok(None) => "running",
                        Err(_) => "error",
                    };
                    eprintln!(
                        "Java probe timeout observation: pid={leader_pid:?} elapsed_ms={} largest_gap_ms={} iterations={} stdout_bytes={} stdout_eof={} stderr_bytes={} stderr_eof={} last_leader={} last_tree={} leader_before_termination={}",
                        now.duration_since(started).as_millis(), gap.as_millis(), iterations,
                        stdout_bytes.len(), stdout_done, stderr_bytes.len(), stderr_done,
                        last_leader, last_tree, leader_before_termination,
                    );
                }
            },
            result = output.stdout.read(&mut stdout), if !stdout_done => {
                read_output(result, &stdout, &mut stdout_bytes, stderr_bytes.len(), &mut stdout_done, &mut failure);
            }
            result = output.stderr.read(&mut stderr), if !stderr_done => {
                read_output(result, &stderr, &mut stderr_bytes, stdout_bytes.len(), &mut stderr_done, &mut failure);
            }
            _ = interval.tick() => {}
        }
        if failure.is_some() {
            // Retry transient termination failures without releasing containment.
            let _ = process.terminate();
        }
        match process.try_wait() {
            Ok(Some(status)) => {
                #[cfg(test)]
                {
                    last_leader = if status.success() {
                        "success"
                    } else {
                        "failure"
                    };
                }
                if !status.success() && failure.is_none() {
                    failure = Some(JavaDiscoveryError::Failed);
                }
                // A diagnostic must not leave descendants behind, even if its
                // leader exits successfully before the pipes reach EOF.
                let _ = process.terminate();
                if stdout_done && stderr_done {
                    let settled = process.tree_settled();
                    #[cfg(test)]
                    {
                        last_tree = match &settled {
                            Ok(true) => "settled",
                            Ok(false) => "unsettled",
                            Err(_) => "error",
                        };
                    }
                    if matches!(settled, Ok(true)) {
                        break;
                    }
                }
            }
            Ok(None) => {
                #[cfg(test)]
                {
                    last_leader = "running";
                }
            }
            Err(_) => {
                #[cfg(test)]
                {
                    last_leader = "error";
                }
                failure.get_or_insert(JavaDiscoveryError::Failed);
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    // Preserve line boundaries between the two independently drained streams.
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&stderr_bytes),
        String::from_utf8_lossy(&stdout_bytes)
    ))
}

fn read_output(
    result: std::io::Result<usize>,
    buffer: &[u8],
    retained: &mut Vec<u8>,
    other_len: usize,
    done: &mut bool,
    failure: &mut Option<JavaDiscoveryError>,
) {
    match result {
        Ok(0) => *done = true,
        Ok(count) if retained.len() + other_len + count <= MAX_OUTPUT_BYTES => {
            if failure.is_none() {
                retained.extend_from_slice(&buffer[..count]);
            }
        }
        Ok(_) => {
            failure.get_or_insert(JavaDiscoveryError::OutputLimit);
        }
        Err(_) => {
            *done = true;
            failure.get_or_insert(JavaDiscoveryError::Failed);
        }
    }
}

fn spawn_error(error: &std::io::Error) -> JavaDiscoveryError {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) && error.raw_os_error() == Some(86) {
        return JavaDiscoveryError::RosettaRequired;
    }
    JavaDiscoveryError::Failed
}

fn fingerprint(path: &Path) -> Result<ExecutableIdentity, JavaDiscoveryError> {
    if !path.is_absolute() {
        return Err(JavaDiscoveryError::InvalidPath);
    }
    let requested_file = fingerprint_file(path)?;
    let selected = console_executable(path);
    let selected_file = if selected == path {
        None
    } else {
        Some(fingerprint_file(&selected)?)
    };
    if console_executable(path) != selected {
        return Err(JavaDiscoveryError::Replaced);
    }
    Ok(ExecutableIdentity {
        requested: path.to_owned(),
        selected,
        requested_file,
        selected_file,
    })
}

fn console_executable(path: &Path) -> PathBuf {
    if cfg!(windows)
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("javaw.exe"))
    {
        let sibling = path.with_file_name("java.exe");
        if sibling.is_file() {
            return sibling;
        }
    }
    path.to_owned()
}

fn fingerprint_file(path: &Path) -> Result<FileIdentity, JavaDiscoveryError> {
    let canonical = fs::canonicalize(path).map_err(file_error)?;
    let metadata = fs::metadata(&canonical).map_err(file_error)?;
    if !metadata.is_file() {
        return Err(JavaDiscoveryError::NotExecutable);
    }
    if metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err(JavaDiscoveryError::ExecutableTooLarge);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(JavaDiscoveryError::NotExecutable);
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Do not hang opening a FIFO raced over the inspected regular file.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let mut file: File = options.open(&canonical).map_err(file_error)?;
    let before = file.metadata().map_err(file_error)?;
    if !before.is_file() {
        return Err(JavaDiscoveryError::NotExecutable);
    }
    if before.len() > MAX_EXECUTABLE_BYTES {
        return Err(JavaDiscoveryError::ExecutableTooLarge);
    }
    if !same_metadata(&metadata, &before) {
        return Err(JavaDiscoveryError::Replaced);
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 << 10];
    let mut size = 0_u64;
    loop {
        let count = file.read(&mut buffer).map_err(file_error)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > MAX_EXECUTABLE_BYTES {
            return Err(JavaDiscoveryError::ExecutableTooLarge);
        }
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(file_error)?;
    let path_after = fs::metadata(path).map_err(file_error)?;
    if size != before.len()
        || !same_metadata(&before, &after)
        || !same_metadata(&after, &path_after)
        || fs::canonicalize(path).map_err(file_error)? != canonical
    {
        return Err(JavaDiscoveryError::Replaced);
    }
    Ok(FileIdentity {
        canonical,
        size,
        modified: after.modified().map_err(file_error)?,
        sha256: hasher.finalize().into(),
        #[cfg(unix)]
        filesystem_identity: unix_identity(&after),
    })
}

fn same_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.is_file()
        && right.is_file()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && {
            #[cfg(unix)]
            {
                unix_identity(left) == unix_identity(right)
            }
            #[cfg(not(unix))]
            {
                left.permissions() == right.permissions()
            }
        }
}

#[cfg(unix)]
fn unix_identity(metadata: &fs::Metadata) -> (u64, u64, i64, i64, u32) {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.mode(),
    )
}

fn file_error(error: std::io::Error) -> JavaDiscoveryError {
    if error.kind() == std::io::ErrorKind::NotFound {
        JavaDiscoveryError::Missing
    } else {
        JavaDiscoveryError::Failed
    }
}

fn property<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .find_map(|(name, value)| (name.trim() == key).then_some(value.trim()))
}

fn parse_java_version(output: &str) -> Option<(u32, u32)> {
    let version = property(output, "java.version").or_else(|| {
        output.lines().map(str::trim).find_map(|line| {
            let rest = line
                .strip_prefix("openjdk version ")
                .or_else(|| line.strip_prefix("java version "))?;
            rest.strip_prefix('"')?.split('"').next()
        })
    })?;
    // Pre-release/build suffixes are not update numbers (25-ea+12 is
    // version 25, not update 12). Preserve the legacy 1.8.0_312 form.
    let numeric = version.split(['-', '+']).next()?;
    let parts: Vec<_> = numeric.split(['.', '_']).collect();
    let first: u32 = parts.first()?.parse().ok()?;
    let (major, update) = if first == 1 {
        (
            parts.get(1)?.parse().ok()?,
            parts
                .get(3)
                .or_else(|| parts.get(2))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        )
    } else {
        (
            first,
            parts
                .get(2)
                .or_else(|| parts.get(1))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        )
    };
    (major > 0).then_some((major, update))
}

fn detect_distribution(output: &str) -> &'static str {
    let identity = [
        "java.vendor",
        "java.vm.vendor",
        "java.vm.name",
        "java.runtime.name",
        "java.runtime.version",
        "java.vm.version",
    ]
    .into_iter()
    .filter_map(|key| property(output, key))
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_uppercase();
    for (name, needles) in [
        ("graalvm", &["GRAALVM"][..]),
        ("openj9", &["OPENJ9", "SEMERU", "IBM"][..]),
        ("temurin", &["TEMURIN", "ECLIPSE", "ADOPTIUM"][..]),
        ("oracle", &["ORACLE"][..]),
        ("openjdk", &["OPENJDK"][..]),
    ] {
        if needles.iter().any(|needle| identity.contains(needle)) {
            return name;
        }
    }
    "unknown"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_and_java_banners_preserve_version_semantics() {
        assert_eq!(
            parse_java_version("java.version = 1.8.0_312-b07"),
            Some((8, 312))
        );
        assert_eq!(
            parse_java_version("openjdk version \"17.0.10\" 2024-01-16"),
            Some((17, 10))
        );
        assert_eq!(parse_java_version("java.version = 25-ea+12"), Some((25, 0)));
        assert_eq!(parse_java_version("java.version = 25+12"), Some((25, 0)));
        assert_eq!(
            parse_java_version("java.version = 21.0.3+9-LTS"),
            Some((21, 3))
        );
        assert_eq!(parse_java_version("garbage \"21.0.3\""), None);
        assert_eq!(parse_java_version("date 2026-09-08"), None);
        assert_eq!(parse_java_version("java.version = broken"), None);
        assert_eq!(
            detect_distribution("user.dir = /Oracle/GraalVM\njava.vendor = Eclipse Adoptium"),
            "temurin"
        );
        assert_eq!(
            detect_distribution("java.vendor = IBM Corporation\njava.vm.name = Eclipse OpenJ9 VM"),
            "openj9"
        );
        assert_eq!(JavaArchitecture::parse("amd64"), JavaArchitecture::X86_64);
        assert_eq!(JavaArchitecture::parse("arm64"), JavaArchitecture::Aarch64);
    }

    #[cfg(unix)]
    fn script(root: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("java");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    const VALID: &str = "printf 'java.version = 21.0.3\\nos.arch = aarch64\\njava.vendor = Eclipse Adoptium\\n' >&2";

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_probe_binds_version_architecture_and_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = script(root.path(), VALID);
        let receipt = probe_java_runtime(&path, Some("custom")).await.unwrap();
        assert_eq!((receipt.info().major, receipt.info().update), (21, 3));
        assert_eq!(receipt.info().distribution, "temurin");
        assert_eq!(receipt.architecture(), JavaArchitecture::Aarch64);
        assert!(receipt.revalidate().is_ok());
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(receipt.revalidate(), Err(JavaDiscoveryError::Replaced));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retargeted_alias_cannot_reuse_a_receipt() {
        use std::os::unix::fs::symlink;
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let path = script(first.path(), VALID);
        let other = script(second.path(), VALID);
        let alias = first.path().join("java-alias");
        symlink(&path, &alias).unwrap();
        let receipt = probe_java_runtime(&alias, None).await.unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        assert_eq!(receipt.revalidate(), Err(JavaDiscoveryError::Replaced));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_nonzero_and_invalid_output_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe_java_runtime(&root.path().join("missing"), None).await,
            Err(JavaDiscoveryError::Missing)
        ));
        let path = script(root.path(), &format!("{VALID}\nexit 1"));
        assert!(matches!(
            probe_java_runtime(&path, None).await,
            Err(JavaDiscoveryError::Failed)
        ));
        let path = script(root.path(), "echo unrelated >&2");
        assert!(matches!(
            probe_java_runtime(&path, None).await,
            Err(JavaDiscoveryError::InvalidVersion)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_executable_with_missing_interpreter_is_failed() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let interpreter = root.path().join("missing-interpreter");
        assert!(matches!(
            probe_java_runtime(&interpreter, None).await,
            Err(JavaDiscoveryError::Missing)
        ));
        let executable = root.path().join("java");
        let bytes = format!("#!{}\nexit 0\n", interpreter.display()).into_bytes();
        fs::write(&executable, &bytes).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let error = probe_java_runtime(&executable, None).await.unwrap_err();
        assert_eq!(fs::read(&executable).unwrap(), bytes);
        assert!(!interpreter.try_exists().unwrap());
        assert_eq!(error, JavaDiscoveryError::Failed);
        assert_eq!(
            error.to_string(),
            "The selected Java executable could not run."
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_and_flooding_executables_are_terminated() {
        let root = tempfile::tempdir().unwrap();
        let path = script(root.path(), "while :; do :; done");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            probe_with_timeout(
                &path,
                None,
                std::future::pending(),
                Duration::from_millis(50),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(JavaDiscoveryError::TimedOut)));
        let path = script(
            root.path(),
            "while :; do printf '01234567890123456789012345678901234567890123456789'; done",
        );
        let result = tokio::time::timeout(Duration::from_secs(5), probe_java_runtime(&path, None))
            .await
            .unwrap();
        assert!(matches!(result, Err(JavaDiscoveryError::OutputLimit)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_reaps_the_probe_before_returning() {
        let root = tempfile::tempdir().unwrap();
        let path = script(root.path(), "while :; do :; done");
        let result =
            probe_java_runtime_until(&path, None, tokio::time::sleep(Duration::from_millis(40)))
                .await;
        assert!(matches!(result, Err(JavaDiscoveryError::Cancelled)));
    }
}
