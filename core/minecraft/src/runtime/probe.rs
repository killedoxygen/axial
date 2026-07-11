use super::model::{JavaRuntimeInfo, JavaRuntimeLookupError};
use super::rosetta::{is_rosetta_exec_error, rosetta_required_error_for_current_host};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

const JAVA_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const JAVA_RUNTIME_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const JAVA_EXECUTABLE_FINGERPRINT_MAX_BYTES: u64 = 64 << 20;

#[derive(Eq, PartialEq)]
struct JavaExecutableFingerprint {
    canonical_path: PathBuf,
    size: u64,
    modified: SystemTime,
    sha256: [u8; 32],
    #[cfg(unix)]
    mode: u32,
}

#[derive(Eq, PartialEq)]
struct JavaExecutableTargetFingerprint {
    requested_path: PathBuf,
    requested: JavaExecutableFingerprint,
    probe: Option<JavaExecutableFingerprint>,
}

pub struct JavaRuntimeProbeReceipt {
    fingerprint: JavaExecutableTargetFingerprint,
    info: JavaRuntimeInfo,
}

impl JavaRuntimeProbeReceipt {
    pub(super) fn matches_path(&self, java_path: &Path) -> Result<bool, JavaRuntimeLookupError> {
        Ok(fingerprint_java_targets(java_path)? == self.fingerprint)
    }

    pub(super) fn into_info(self) -> JavaRuntimeInfo {
        self.info
    }
}

pub fn probe_java_runtime_info(
    java_path: &Path,
    id_hint: Option<&str>,
) -> Result<JavaRuntimeInfo, JavaRuntimeLookupError> {
    let requested_path = absolute_java_path(java_path)?;
    let exec_path = java_probe_executable(&requested_path);
    let mut command = Command::new(&exec_path);
    command.args(["-XshowSettings:property", "-version"]);
    let output =
        command_output_with_timeout(command, JAVA_RUNTIME_PROBE_TIMEOUT).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                JavaRuntimeLookupError::ProbeTimedOut
            } else if is_rosetta_exec_error(&error)
                && let Some(error) = rosetta_required_error_for_current_host(
                    &exec_path,
                    id_hint.unwrap_or("selected-runtime"),
                )
            {
                error
            } else {
                JavaRuntimeLookupError::Probe(error.to_string())
            }
        })?;

    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let (major, update) = parse_java_version(&text);
    Ok(JavaRuntimeInfo {
        id: id_hint.unwrap_or_default().to_string(),
        major,
        update,
        distribution: detect_distribution(&text),
        path: requested_path.to_string_lossy().to_string(),
    })
}

pub fn probe_java_runtime_receipt(
    java_path: &Path,
    id_hint: Option<&str>,
) -> Result<JavaRuntimeProbeReceipt, JavaRuntimeLookupError> {
    let requested_path = absolute_java_path(java_path)?;
    let before = fingerprint_java_targets(&requested_path)?;
    let info = probe_java_runtime_info(&requested_path, id_hint)?;
    let after = fingerprint_java_targets(&requested_path)?;
    if before != after {
        return Err(JavaRuntimeLookupError::Probe(
            "java executable changed while it was being probed".to_string(),
        ));
    }
    Ok(JavaRuntimeProbeReceipt {
        fingerprint: after,
        info,
    })
}

fn fingerprint_java_targets(
    java_path: &Path,
) -> Result<JavaExecutableTargetFingerprint, JavaRuntimeLookupError> {
    let requested_path = absolute_java_path(java_path)?;
    let requested = fingerprint_java_executable(&requested_path)?;
    let probe_path = java_probe_executable(&requested_path);
    let probe = if probe_path == requested_path {
        None
    } else {
        Some(fingerprint_java_executable(&probe_path)?)
    };
    Ok(JavaExecutableTargetFingerprint {
        requested_path,
        requested,
        probe,
    })
}

fn absolute_java_path(java_path: &Path) -> Result<PathBuf, JavaRuntimeLookupError> {
    std::path::absolute(java_path).map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))
}

fn fingerprint_java_executable(
    java_path: &Path,
) -> Result<JavaExecutableFingerprint, JavaRuntimeLookupError> {
    let canonical_before = fs::canonicalize(java_path)
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
    let before = fingerprint_opened_executable(&canonical_before)?;
    let canonical_after = fs::canonicalize(java_path)
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
    let after = fingerprint_opened_executable(&canonical_after)?;
    if canonical_before != canonical_after || before != after {
        return Err(JavaRuntimeLookupError::Probe(
            "java executable changed while it was fingerprinted".to_string(),
        ));
    }
    Ok(after)
}

fn fingerprint_opened_executable(
    canonical_path: &Path,
) -> Result<JavaExecutableFingerprint, JavaRuntimeLookupError> {
    let mut file = File::open(canonical_path)
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
    let metadata_before = file
        .metadata()
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
    if !metadata_before.is_file() {
        return Err(JavaRuntimeLookupError::Probe(
            "java executable is not a regular file".to_string(),
        ));
    }
    if metadata_before.len() > JAVA_EXECUTABLE_FINGERPRINT_MAX_BYTES {
        return Err(JavaRuntimeLookupError::Probe(
            "java executable exceeds the fingerprint size limit".to_string(),
        ));
    }
    let modified_before = metadata_before
        .modified()
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
    #[cfg(unix)]
    let mode_before = {
        use std::os::unix::fs::PermissionsExt as _;
        metadata_before.permissions().mode()
    };
    let mut hasher = Sha256::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 16 << 10];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
        if count == 0 {
            break;
        }
        read = read.saturating_add(count as u64);
        if read > JAVA_EXECUTABLE_FINGERPRINT_MAX_BYTES {
            return Err(JavaRuntimeLookupError::Probe(
                "java executable exceeds the fingerprint size limit".to_string(),
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let metadata_after = file
        .metadata()
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
    let modified_after = metadata_after
        .modified()
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
    #[cfg(unix)]
    let mode_after = {
        use std::os::unix::fs::PermissionsExt as _;
        metadata_after.permissions().mode()
    };
    #[cfg(unix)]
    let mode_changed = mode_after != mode_before;
    #[cfg(not(unix))]
    let mode_changed = false;
    if !metadata_after.is_file()
        || read != metadata_before.len()
        || metadata_after.len() != metadata_before.len()
        || modified_after != modified_before
        || mode_changed
    {
        return Err(JavaRuntimeLookupError::Probe(
            "java executable changed while it was fingerprinted".to_string(),
        ));
    }
    Ok(JavaExecutableFingerprint {
        canonical_path: canonical_path.to_path_buf(),
        size: metadata_after.len(),
        modified: modified_after,
        sha256: hasher.finalize().into(),
        #[cfg(unix)]
        mode: mode_after,
    })
}
pub(super) fn java_probe_executable(java_path: &Path) -> PathBuf {
    if !cfg!(target_os = "windows") {
        return java_path.to_path_buf();
    }

    if java_path
        .file_name()
        .map(|name| name.to_string_lossy().eq_ignore_ascii_case("javaw.exe"))
        .unwrap_or(false)
    {
        let candidate = java_path.with_file_name("java.exe");
        if candidate.is_file() {
            return candidate;
        }
    }

    java_path.to_path_buf()
}

fn command_output_with_timeout(mut command: Command, timeout: Duration) -> std::io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;

    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }

        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "java runtime probe timed out",
            ));
        }

        std::thread::sleep(JAVA_RUNTIME_PROBE_POLL_INTERVAL.min(deadline - now));
    }
}

pub(super) fn parse_java_version(text: &str) -> (u32, u32) {
    let Some(version) = text
        .lines()
        .find_map(|line| line.split('"').nth(1))
        .or_else(|| {
            text.split_whitespace()
                .find(|token| token.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        })
    else {
        return (0, 0);
    };

    let parts = version
        .split(['.', '_', '-', '+'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return (0, 0);
    }

    if parts[0] == "1" {
        let major = parts
            .get(1)
            .and_then(|part| part.parse::<u32>().ok())
            .unwrap_or_default();
        let update_index = if parts.get(2).is_some_and(|part| *part == "0") {
            3
        } else {
            2
        };
        let update = parts
            .get(update_index)
            .and_then(|part| part.parse::<u32>().ok())
            .unwrap_or_default();
        return (major, update);
    }

    let major = parts[0].parse::<u32>().ok().unwrap_or_default();
    let update = parts
        .get(2)
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or_else(|| {
            parts
                .get(1)
                .and_then(|part| part.parse::<u32>().ok())
                .unwrap_or_default()
        });
    (major, update)
}

pub(super) fn detect_distribution(text: &str) -> String {
    const IDENTITY_PROPERTIES: [&str; 6] = [
        "java.vendor",
        "java.vm.vendor",
        "java.vm.name",
        "java.runtime.name",
        "java.runtime.version",
        "java.vm.version",
    ];

    let identities = text
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .filter_map(|(key, value)| {
            let key = key.trim();
            IDENTITY_PROPERTIES
                .iter()
                .any(|property| key.eq_ignore_ascii_case(property))
                .then(|| value.trim().to_uppercase())
        })
        .collect::<Vec<_>>();

    let contains_identity = |needles: &[&str]| {
        identities
            .iter()
            .any(|identity| needles.iter().any(|needle| identity.contains(needle)))
    };

    match () {
        _ if contains_identity(&["GRAALVM"]) => "graalvm".to_string(),
        _ if contains_identity(&["OPENJ9", "SEMERU", "IBM"]) => "openj9".to_string(),
        _ if contains_identity(&["TEMURIN", "ECLIPSE", "ADOPTIUM"]) => "temurin".to_string(),
        _ if contains_identity(&["ORACLE"]) => "oracle".to_string(),
        _ if contains_identity(&["OPENJDK"]) => "openjdk".to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{command_output_with_timeout, parse_java_version, probe_java_runtime_receipt};
    use std::fs;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn java8_version_parser_uses_update_after_zero_component() {
        assert_eq!(
            parse_java_version(r#"openjdk version "1.8.0_311""#),
            (8, 311)
        );
        assert_eq!(
            parse_java_version(r#"openjdk version "1.8.0_312-b07""#),
            (8, 312)
        );
    }

    #[test]
    fn modern_java_version_parser_keeps_major_and_update() {
        assert_eq!(
            parse_java_version(r#"openjdk version "17.0.10" 2024-01-16"#),
            (17, 10)
        );
    }

    #[cfg(unix)]
    #[test]
    fn java_probe_command_output_is_bounded_by_timeout() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("axial-java-probe-timeout-{}", std::process::id()));
        fs::create_dir_all(&root).expect("probe timeout test dir");
        let java_path = root.join("java");
        fs::write(&java_path, "#!/bin/sh\nsleep 60\n").expect("probe timeout script");
        let mut permissions = fs::metadata(&java_path)
            .expect("probe timeout metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("probe timeout executable");

        let started = Instant::now();
        let error =
            command_output_with_timeout(Command::new(&java_path), Duration::from_millis(50))
                .expect_err("probe command should time out");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn receipt_binds_the_requested_alias_and_symlink_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!(
            "axial-java-probe-receipt-alias-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("receipt alias root");
        let first = root.join("java-first");
        let second = root.join("java-second");
        for path in [&first, &second] {
            fs::write(path, "#!/bin/sh\necho 'openjdk version \"21.0.3\"' >&2\n")
                .expect("fake java");
            let mut permissions = fs::metadata(path)
                .expect("fake java metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("fake java permissions");
        }
        let first_alias = root.join("java-alias-a");
        let second_alias = root.join("java-alias-b");
        symlink(&first, &first_alias).expect("first alias");
        symlink(&first, &second_alias).expect("second alias");

        let receipt = probe_java_runtime_receipt(&first_alias, None).expect("probe receipt");
        assert!(receipt.matches_path(&first_alias).expect("matching alias"));
        assert!(
            !receipt
                .matches_path(&second_alias)
                .expect("different alias")
        );

        fs::remove_file(&first_alias).expect("remove old alias");
        symlink(&second, &first_alias).expect("retarget alias");
        assert!(
            !receipt
                .matches_path(&first_alias)
                .expect("retargeted alias")
        );
        let _ = fs::remove_dir_all(root);
    }
}
