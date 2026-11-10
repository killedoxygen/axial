//! Bounded Java diagnostics bound to immutable executable evidence.
//!
//! Cancellation terminates the contained tree; the probe still returns only
//! after that tree is reaped, so callers retain it in an owned task.

use super::model::JavaRuntimeInfo;
use super::rosetta::{is_rosetta_exec_error, rosetta_required_error_for_current_host};
use crate::process::{OwnedProcess, SpawnError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const MAX_EXECUTABLE_BYTES: u64 = 64 << 20;
const MAX_OUTPUT_BYTES: usize = 64 << 10;
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JavaArchitecture {
    X86,
    X86_64,
    Aarch64,
    Unknown,
}

impl JavaArchitecture {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "x86" | "i386" | "i486" | "i586" | "i686" => Self::X86,
            "amd64" | "x86_64" | "x64" => Self::X86_64,
            "aarch64" | "arm64" => Self::Aarch64,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum JavaProbeError {
    #[error("java executable is missing")]
    Missing,
    #[error("java executable path is not absolute")]
    InvalidPath,
    #[error("java executable is not an executable regular file")]
    NotExecutable,
    #[error("java executable exceeds the fingerprint size limit")]
    ExecutableTooLarge,
    #[error("java executable changed after it was fingerprinted")]
    Replaced,
    #[error("java probe timed out")]
    TimedOut,
    #[error("java probe output exceeded its limit")]
    OutputLimit,
    #[error("java did not report a valid version")]
    InvalidVersion,
    #[error("java executable could not run")]
    Failed,
    #[error("java probe was cancelled")]
    Cancelled,
    #[error("java executable requires Rosetta 2")]
    RosettaRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    canonical: PathBuf,
    size: u64,
    modified: SystemTime,
    sha256: [u8; 32],
    #[cfg(unix)]
    filesystem: (u64, u64, i64, i64, u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutableIdentity {
    requested: PathBuf,
    /// The console sibling actually run for diagnostics and processors.
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
        &self.identity.requested
    }

    /// Rechecks the requested alias, the selected console sibling, metadata
    /// and digest immediately before spawn. This never starts a process.
    pub fn revalidate(&self) -> Result<(), JavaProbeError> {
        match fingerprint(&self.identity.requested) {
            Ok(identity) if identity == self.identity => Ok(()),
            _ => Err(JavaProbeError::Replaced),
        }
    }

    pub fn revalidate_cli_executable(&self) -> Result<PathBuf, JavaProbeError> {
        self.revalidate()?;
        Ok(self.identity.selected.clone())
    }
}

pub async fn probe_java(
    path: &Path,
    id_hint: Option<&str>,
    cancellation: impl Future<Output = ()>,
) -> Result<JavaProbeReceipt, JavaProbeError> {
    probe_with_timeout(path, id_hint, cancellation, PROBE_TIMEOUT).await
}

async fn probe_with_timeout(
    path: &Path,
    id_hint: Option<&str>,
    cancellation: impl Future<Output = ()>,
    timeout: Duration,
) -> Result<JavaProbeReceipt, JavaProbeError> {
    let identity = fingerprint(path)?;
    let output = run(&identity.selected, cancellation, timeout).await?;
    if fingerprint(path).map_err(|_| JavaProbeError::Replaced)? != identity {
        return Err(JavaProbeError::Replaced);
    }
    let (major, update) = parse_java_version(&output).ok_or(JavaProbeError::InvalidVersion)?;
    Ok(JavaProbeReceipt {
        info: JavaRuntimeInfo {
            id: id_hint.unwrap_or_default().to_owned(),
            major,
            update,
            distribution: detect_distribution(&output).to_owned(),
            path: identity.requested.to_string_lossy().into_owned(),
        },
        identity,
        architecture: property(&output, "os.arch")
            .map_or(JavaArchitecture::Unknown, JavaArchitecture::parse),
    })
}

async fn run(
    executable: &Path,
    cancellation: impl Future<Output = ()>,
    timeout: Duration,
) -> Result<String, JavaProbeError> {
    let mut command = Command::new(executable);
    command.args(["-XshowSettings:properties", "-version"]);
    // Launcher-injected JVM options must not run agents or alter this diagnostic.
    for key in [
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
    ] {
        command.env_remove(key);
    }
    let (mut process, output) = match OwnedProcess::spawn(command) {
        Ok(spawned) => spawned,
        Err(SpawnError::BeforeSpawn(error)) => return Err(spawn_error(executable, &error)),
        Err(SpawnError::Unsettled(mut process, _, _)) => {
            process.settle().await;
            return Err(JavaProbeError::Failed);
        }
    };
    let completed = async {
        let (stderr, stdout) =
            tokio::try_join!(read_bounded(output.stderr), read_bounded(output.stdout))?;
        if stderr.len() + stdout.len() > MAX_OUTPUT_BYTES {
            return Err(JavaProbeError::OutputLimit);
        }
        match process.wait().await {
            Ok(status) if status.success() => Ok(format!(
                "{}\n{}",
                String::from_utf8_lossy(&stderr),
                String::from_utf8_lossy(&stdout)
            )),
            _ => Err(JavaProbeError::Failed),
        }
    };
    let result = tokio::select! {
        biased;
        () = cancellation => Err(JavaProbeError::Cancelled),
        () = tokio::time::sleep(timeout) => Err(JavaProbeError::TimedOut),
        result = completed => result,
    };
    // A diagnostic leaves no descendants behind, even after its leader succeeds.
    process.settle().await;
    result
}

async fn read_bounded(stream: impl AsyncRead + Unpin) -> Result<Vec<u8>, JavaProbeError> {
    let mut bytes = Vec::new();
    stream
        .take(MAX_OUTPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| JavaProbeError::Failed)?;
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err(JavaProbeError::OutputLimit);
    }
    Ok(bytes)
}

fn spawn_error(executable: &Path, error: &std::io::Error) -> JavaProbeError {
    if is_rosetta_exec_error(error)
        && rosetta_required_error_for_current_host(executable, "").is_some()
    {
        JavaProbeError::RosettaRequired
    } else {
        JavaProbeError::Failed
    }
}

fn fingerprint(path: &Path) -> Result<ExecutableIdentity, JavaProbeError> {
    if !path.is_absolute() {
        return Err(JavaProbeError::InvalidPath);
    }
    let requested_file = fingerprint_file(path)?;
    let selected = console_executable(path);
    let selected_file = if selected == path {
        None
    } else {
        Some(fingerprint_file(&selected)?)
    };
    if console_executable(path) != selected {
        return Err(JavaProbeError::Replaced);
    }
    Ok(ExecutableIdentity {
        requested: path.to_owned(),
        selected,
        requested_file,
        selected_file,
    })
}

/// Windows diagnostics and processors need the console `java.exe` beside a
/// selected `javaw.exe`; elsewhere the requested executable runs as is.
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

fn fingerprint_file(path: &Path) -> Result<FileIdentity, JavaProbeError> {
    let canonical = fs::canonicalize(path).map_err(file_error)?;
    let metadata = fs::metadata(&canonical).map_err(file_error)?;
    if !metadata.is_file() {
        return Err(JavaProbeError::NotExecutable);
    }
    if metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err(JavaProbeError::ExecutableTooLarge);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(JavaProbeError::NotExecutable);
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use rustix::fs::OFlags;
        use std::os::unix::fs::OpenOptionsExt as _;
        // Do not hang opening a FIFO raced over the inspected regular file.
        options.custom_flags((OFlags::NONBLOCK | OFlags::NOFOLLOW).bits() as i32);
    }
    let mut file: File = options.open(&canonical).map_err(file_error)?;
    let before = file.metadata().map_err(file_error)?;
    if !before.is_file() {
        return Err(JavaProbeError::NotExecutable);
    }
    if !same_metadata(&metadata, &before) {
        return Err(JavaProbeError::Replaced);
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
            return Err(JavaProbeError::ExecutableTooLarge);
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
        return Err(JavaProbeError::Replaced);
    }
    Ok(FileIdentity {
        canonical,
        size,
        modified: after.modified().map_err(file_error)?,
        sha256: hasher.finalize().into(),
        #[cfg(unix)]
        filesystem: unix_identity(&after),
    })
}

fn same_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    let same_identity = unix_identity(left) == unix_identity(right);
    #[cfg(not(unix))]
    let same_identity = left.permissions() == right.permissions();
    left.is_file()
        && right.is_file()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && same_identity
}

#[cfg(unix)]
fn unix_identity(metadata: &fs::Metadata) -> (u64, u64, i64, i64, u32) {
    use std::os::unix::fs::MetadataExt as _;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.mode(),
    )
}

fn file_error(error: std::io::Error) -> JavaProbeError {
    if error.kind() == std::io::ErrorKind::NotFound {
        JavaProbeError::Missing
    } else {
        JavaProbeError::Failed
    }
}

fn property<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .find_map(|(name, value)| (name.trim() == key).then_some(value.trim()))
}

/// The `java.version` property, else the `openjdk|java [version] "X"` banner.
/// Pre-release and build suffixes are not updates: `22-ea+27` is Java 22.0.
/// Legacy `1.8.0_312` is Java 8 update 312; JEP 322 `17.0.2` is 17 update 2.
fn parse_java_version(output: &str) -> Option<(u32, u32)> {
    let version = property(output, "java.version").or_else(|| {
        output.lines().find_map(|line| {
            let line = line.trim_start();
            let rest = line
                .strip_prefix("openjdk ")
                .or_else(|| line.strip_prefix("java "))?;
            let rest = rest.strip_prefix("version ").unwrap_or(rest);
            let version = rest.split_whitespace().next()?.trim_matches('"');
            version
                .starts_with(|c: char| c.is_ascii_digit())
                .then_some(version)
        })
    })?;
    let numbers = version
        .split(['-', '+'])
        .next()?
        .split(['.', '_'])
        .map(|part| part.parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    let (major, rest) = match numbers.as_slice() {
        [1, major, rest @ ..] | [major, rest @ ..] => (*major, rest),
        [] => return None,
    };
    (major > 0).then(|| (major, rest.get(1).copied().unwrap_or(0)))
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
    [
        ("graalvm", &["GRAALVM"][..]),
        ("openj9", &["OPENJ9", "SEMERU", "IBM"][..]),
        ("temurin", &["TEMURIN", "ECLIPSE", "ADOPTIUM"][..]),
        ("oracle", &["ORACLE"][..]),
        ("openjdk", &["OPENJDK"][..]),
    ]
    .into_iter()
    .find(|(_, needles)| needles.iter().any(|needle| identity.contains(needle)))
    .map_or("unknown", |(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_separates_major_update_and_suffixes() {
        let notices = "Security settings summary:\n    See \"java -X\" for verbose security settings options\njava -X\nopenjdk help";
        for (output, expected) in [
            ("java.version = 1.8.0_312-b07", Some((8, 312))),
            (r#"java version "1.8.0_311""#, Some((8, 311))),
            ("java.version = 1.8.0", Some((8, 0))),
            ("java.version = 17.0.2", Some((17, 2))),
            ("java.version = 21-ea", Some((21, 0))),
            ("java.version = 22-ea+27", Some((22, 0))),
            ("java.version = 25+12", Some((25, 0))),
            ("java.version = 21.0.3+9-LTS", Some((21, 3))),
            (r#"openjdk version "17.0.10" 2024-01-16"#, Some((17, 10))),
            ("openjdk 21.0.8 2025-07-15", Some((21, 8))),
            (
                &format!("{notices}\nopenjdk version \"17.0.15\" 2025-04-15 LTS"),
                Some((17, 15)),
            ),
            (
                "java.version = 21.0.3\nopenjdk version \"17.0.1\"",
                Some((21, 3)),
            ),
            ("garbage \"21.0.3\"", None),
            ("date 2026-09-08", None),
            ("java.version = broken", None),
            ("java.version = 1.8.0_x", None),
            (notices, None),
        ] {
            assert_eq!(parse_java_version(output), expected, "{output}");
        }
    }

    #[test]
    fn distribution_and_architecture_classify_identity_properties() {
        for (output, expected) in [
            (
                "java.vendor = Oracle Corporation\njava.vm.name = GraalVM 64-Bit Server VM",
                "graalvm",
            ),
            (
                "java.vendor = OpenJDK\njava.runtime.name = GraalVM Runtime Environment",
                "graalvm",
            ),
            (
                "java.vendor = IBM Corporation\njava.vm.name = Eclipse OpenJ9 VM",
                "openj9",
            ),
            (
                "java.runtime.name = IBM Semeru Runtime Open Edition",
                "openj9",
            ),
            (
                "java.runtime.name = OpenJDK Runtime Environment Temurin-21.0.2+13",
                "temurin",
            ),
            (
                "user.dir = /Oracle/GraalVM\njava.vendor = Eclipse Adoptium",
                "temurin",
            ),
            ("java.vm.vendor = Eclipse Foundation", "temurin"),
            (
                "java.vendor   =   Oracle Corporation\njava.vm.name = Java HotSpot(TM) 64-Bit Server VM",
                "oracle",
            ),
            (
                "java.vendor = Debian\njava.vm.name = OpenJDK 64-Bit Server VM\njava.runtime.version = 21.0.5+11-Debian-1",
                "openjdk",
            ),
            ("java.home = /opt/java\nsun.arch.data.model = 64", "unknown"),
        ] {
            assert_eq!(detect_distribution(output), expected, "{output}");
        }
        assert_eq!(JavaArchitecture::parse("amd64"), JavaArchitecture::X86_64);
        assert_eq!(
            JavaArchitecture::parse(" ARM64 "),
            JavaArchitecture::Aarch64
        );
        assert_eq!(JavaArchitecture::parse("i686"), JavaArchitecture::X86);
        assert_eq!(
            JavaArchitecture::parse("ppc64le"),
            JavaArchitecture::Unknown
        );
    }

    #[cfg(windows)]
    #[test]
    fn receipt_selects_the_fingerprinted_console_sibling_and_refuses_a_raced_one() {
        let root = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let javaw = root.path().join("javaw.exe");
        let java = root.path().join("java.exe");
        fs::write(&javaw, b"windowed executable").unwrap();
        let receipt = |identity| JavaProbeReceipt {
            identity,
            info: JavaRuntimeInfo {
                id: String::new(),
                major: 21,
                update: 0,
                distribution: String::new(),
                path: String::new(),
            },
            architecture: JavaArchitecture::X86_64,
        };
        let windowed = receipt(fingerprint(&javaw).unwrap());
        assert_eq!(windowed.revalidate_cli_executable(), Ok(javaw.clone()));
        fs::write(&java, b"raced console executable").unwrap();
        assert_eq!(windowed.revalidate(), Err(JavaProbeError::Replaced));
        let console = receipt(fingerprint(&javaw).unwrap());
        assert_eq!(console.revalidate_cli_executable(), Ok(java.clone()));
        fs::write(&java, b"changed console executable").unwrap();
        assert_eq!(console.revalidate(), Err(JavaProbeError::Replaced));
    }

    #[cfg(unix)]
    fn script(root: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = root.join("java");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    const VALID: &str = "printf 'java.version = 21.0.3\\nos.arch = aarch64\\njava.vendor = Eclipse Adoptium\\n' >&2";

    #[cfg(unix)]
    async fn probe(path: &Path) -> Result<JavaProbeReceipt, JavaProbeError> {
        probe_java(path, None, std::future::pending()).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_probe_binds_version_architecture_and_exact_bytes() {
        let root = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let path = script(root.path(), VALID);
        let receipt = probe_java(&path, Some("custom"), std::future::pending())
            .await
            .unwrap();
        assert_eq!((receipt.info().major, receipt.info().update), (21, 3));
        assert_eq!(receipt.info().id, "custom");
        assert_eq!(receipt.info().distribution, "temurin");
        assert_eq!(receipt.architecture(), JavaArchitecture::Aarch64);
        assert_eq!(receipt.revalidate_cli_executable(), Ok(path.clone()));
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(receipt.revalidate(), Err(JavaProbeError::Replaced));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retargeted_alias_cannot_reuse_a_receipt() {
        use std::os::unix::fs::symlink;
        let first = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let second = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let path = script(first.path(), VALID);
        let other = script(second.path(), VALID);
        let alias = first.path().join("java-alias");
        symlink(&path, &alias).unwrap();
        let receipt = probe(&alias).await.unwrap();
        assert_eq!(receipt.revalidate_cli_executable(), Ok(alias.clone()));
        fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        assert_eq!(receipt.revalidate(), Err(JavaProbeError::Replaced));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_nonzero_invalid_and_uninterpretable_executables_are_distinct() {
        let root = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        assert_eq!(
            probe(Path::new("relative/java")).await.unwrap_err(),
            JavaProbeError::InvalidPath
        );
        let interpreter = root.path().join("missing-interpreter");
        assert_eq!(
            probe(&interpreter).await.unwrap_err(),
            JavaProbeError::Missing
        );
        let path = script(root.path(), &format!("{VALID}\nexit 1"));
        assert_eq!(probe(&path).await.unwrap_err(), JavaProbeError::Failed);
        let path = script(root.path(), "echo unrelated >&2");
        assert_eq!(
            probe(&path).await.unwrap_err(),
            JavaProbeError::InvalidVersion
        );

        use std::os::unix::fs::PermissionsExt as _;
        let bytes = format!("#!{}\nexit 0\n", interpreter.display()).into_bytes();
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(probe(&path).await.unwrap_err(), JavaProbeError::Failed);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!interpreter.try_exists().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_flooding_and_cancelled_probes_are_terminated() {
        let root = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let hanging = script(root.path(), "sleep 60 & while :; do :; done");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            probe_with_timeout(
                &hanging,
                None,
                std::future::pending(),
                Duration::from_millis(50),
            ),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err(), JavaProbeError::TimedOut);
        let cancelled = tokio::time::timeout(
            Duration::from_secs(5),
            probe_java(
                &hanging,
                None,
                tokio::time::sleep(Duration::from_millis(40)),
            ),
        )
        .await
        .unwrap();
        assert_eq!(cancelled.unwrap_err(), JavaProbeError::Cancelled);
        let flooding = script(
            root.path(),
            "while :; do printf '01234567890123456789012345678901234567890123456789'; done",
        );
        let result = tokio::time::timeout(Duration::from_secs(5), probe(&flooding))
            .await
            .unwrap();
        assert_eq!(result.unwrap_err(), JavaProbeError::OutputLimit);
    }
}
