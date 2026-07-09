use super::model::{JavaRuntimeInfo, JavaRuntimeLookupError};
use super::rosetta::{is_rosetta_exec_error, rosetta_required_error_for_current_host};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const JAVA_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const JAVA_RUNTIME_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub fn probe_java_runtime_info(
    java_path: &Path,
    id_hint: Option<&str>,
) -> Result<JavaRuntimeInfo, JavaRuntimeLookupError> {
    let exec_path = java_probe_executable(java_path);
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
        path: java_path.to_string_lossy().to_string(),
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
    use super::{command_output_with_timeout, parse_java_version};
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
            std::env::temp_dir().join(format!("croopor-java-probe-timeout-{}", std::process::id()));
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
}
