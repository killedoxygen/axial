use crate::GuardianMode;
use crate::build::{find_client_jar, uses_module_bootstrap};
use croopor_minecraft::{
    LaunchModelError, RuntimeOverride, VersionJson, default_environment, libraries_dir,
    load_version_json, parse_runtime_override, preferred_runtime_component, resolve_libraries,
    resolve_version, runtime_component_ready_without_probe, runtime_executable_ready_without_probe,
};
use serde::Serialize;
use sha1::{Digest as _, Sha1};
use std::collections::HashSet;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct LaunchReadinessRequest {
    pub library_dir: PathBuf,
    pub version_id: String,
    pub requested_java: String,
    pub guardian_mode: GuardianMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LaunchReadiness {
    pub launchable: bool,
    pub reasons: Vec<LaunchReadinessReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LaunchReadinessReason {
    pub id: LaunchReadinessReasonId,
    pub severity: LaunchReadinessSeverity,
    pub message: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchReadinessReasonId {
    VersionJsonMissing,
    ParentVersionMissing,
    IncompleteInstall,
    ClientJarMissing,
    ClientJarCorrupt,
    LibrariesMissing,
    LibrariesCorrupt,
    AssetIndexMissing,
    AssetIndexCorrupt,
    ManagedRuntimeMissing,
    JavaOverrideMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchReadinessSeverity {
    Blocking,
    Recoverable,
}

pub fn inspect_launch_readiness(request: &LaunchReadinessRequest) -> LaunchReadiness {
    let mut reasons = Vec::new();
    inspect_incomplete_install_markers(&request.library_dir, &request.version_id, &mut reasons);

    let version = match resolve_version(&request.library_dir, &request.version_id) {
        Ok(version) => {
            inspect_version_files(
                &request.library_dir,
                &request.version_id,
                &version,
                &mut reasons,
            );
            Some(version)
        }
        Err(error) => {
            reasons.push(reason_for_version_error(&error));
            None
        }
    };

    inspect_runtime_files(request, version.as_ref(), &mut reasons);

    LaunchReadiness {
        launchable: reasons
            .iter()
            .all(|reason| reason.severity != LaunchReadinessSeverity::Blocking),
        reasons,
    }
}

fn inspect_incomplete_install_markers(
    library_dir: &Path,
    version_id: &str,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    let mut current_id = version_id.trim().to_string();
    let mut seen = HashSet::new();
    let mut depth = 0;

    while !current_id.is_empty() && seen.insert(current_id.clone()) && depth <= 10 {
        let version_dir = library_dir.join("versions").join(&current_id);
        if version_dir.join(".incomplete").exists() {
            reasons.push(reason(
                LaunchReadinessReasonId::IncompleteInstall,
                "Installation is incomplete. Finish or repair this version before launching.",
                LaunchReadinessSeverity::Blocking,
            ));
            return;
        }

        let Ok(version) = load_version_json(library_dir, &current_id) else {
            return;
        };
        current_id = version.inherits_from.trim().to_string();
        depth += 1;
    }
}

fn inspect_version_files(
    library_dir: &Path,
    version_id: &str,
    version: &VersionJson,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    if !uses_module_bootstrap(version) {
        match find_client_jar(library_dir, version, version_id) {
            Some(client_jar)
                if artifact_corrupt(&client_jar, version.downloads.client.as_ref()) =>
            {
                reasons.push(reason(
                    LaunchReadinessReasonId::ClientJarCorrupt,
                    "Client game files are corrupt. Repair this version before launching.",
                    LaunchReadinessSeverity::Blocking,
                ));
            }
            Some(_) => {}
            None => {
                reasons.push(reason(
                    LaunchReadinessReasonId::ClientJarMissing,
                    "Client game files are missing. Install this version before launching.",
                    LaunchReadinessSeverity::Blocking,
                ));
            }
        }
    }

    let libraries = resolve_libraries(version, library_dir, &default_environment());
    if libraries.iter().any(|library| !library.abs_path.is_file()) {
        reasons.push(reason(
            LaunchReadinessReasonId::LibrariesMissing,
            "Required libraries are missing. Install this version before launching.",
            LaunchReadinessSeverity::Blocking,
        ));
    } else if libraries.iter().any(|library| {
        library_integrity_expectation(version, library_dir, &library.abs_path)
            .is_some_and(|expected| expected.path_corrupt(&library.abs_path))
    }) {
        reasons.push(reason(
            LaunchReadinessReasonId::LibrariesCorrupt,
            "Required libraries are corrupt. Repair this version before launching.",
            LaunchReadinessSeverity::Blocking,
        ));
    }

    if !version.asset_index.id.trim().is_empty() {
        let asset_index_path = library_dir
            .join("assets")
            .join("indexes")
            .join(format!("{}.json", version.asset_index.id));
        if !asset_index_path.is_file() {
            reasons.push(reason(
                LaunchReadinessReasonId::AssetIndexMissing,
                "Asset index is missing. Install this version before launching.",
                LaunchReadinessSeverity::Blocking,
            ));
        } else if ArtifactIntegrityExpectation::new(
            version.asset_index.size,
            &version.asset_index.sha1,
        )
        .is_some_and(|expected| expected.path_corrupt(&asset_index_path))
        {
            reasons.push(reason(
                LaunchReadinessReasonId::AssetIndexCorrupt,
                "Asset index is corrupt. Repair this version before launching.",
                LaunchReadinessSeverity::Blocking,
            ));
        }
    }
}

#[derive(Clone, Debug)]
struct ArtifactIntegrityExpectation {
    size: Option<u64>,
    sha1: Option<String>,
}

impl ArtifactIntegrityExpectation {
    fn new(size: i64, sha1: &str) -> Option<Self> {
        let size = (size >= 0).then_some(size as u64).filter(|size| *size > 0);
        let sha1 = is_sha1_hex(sha1).then(|| sha1.to_ascii_lowercase());
        (size.is_some() || sha1.is_some()).then_some(Self { size, sha1 })
    }

    fn path_corrupt(&self, path: &Path) -> bool {
        let Ok(metadata) = std::fs::metadata(path) else {
            return true;
        };
        if !metadata.is_file() {
            return true;
        }
        if let Some(expected_size) = self.size
            && metadata.len() != expected_size
        {
            return true;
        }
        if self.sha1.is_some() {
            return hash_file_sha1(path)
                .ok()
                .is_none_or(|actual| Some(actual) != self.sha1);
        }
        false
    }
}

fn artifact_corrupt(path: &Path, entry: Option<&croopor_minecraft::launch::DownloadEntry>) -> bool {
    entry
        .and_then(|entry| ArtifactIntegrityExpectation::new(entry.size, &entry.sha1))
        .is_some_and(|expected| expected.path_corrupt(path))
}

fn library_integrity_expectation(
    version: &VersionJson,
    library_dir: &Path,
    abs_path: &Path,
) -> Option<ArtifactIntegrityExpectation> {
    let libraries_root = libraries_dir(library_dir);
    let relative_path = abs_path.strip_prefix(&libraries_root).ok()?;
    let relative_path = relative_path.to_string_lossy().replace('\\', "/");

    for library in &version.libraries {
        let Some(downloads) = library.downloads.as_ref() else {
            continue;
        };
        if let Some(artifact) = downloads.artifact.as_ref()
            && artifact.path == relative_path
        {
            return ArtifactIntegrityExpectation::new(artifact.size, &artifact.sha1);
        }
        for artifact in downloads.classifiers.values() {
            if artifact.path == relative_path {
                return ArtifactIntegrityExpectation::new(artifact.size, &artifact.sha1);
            }
        }
    }

    None
}

fn hash_file_sha1(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn is_sha1_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn inspect_runtime_files(
    request: &LaunchReadinessRequest,
    version: Option<&VersionJson>,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    let selected_override = parse_runtime_override(&request.requested_java);
    if matches!(request.guardian_mode, GuardianMode::Custom) {
        match selected_override {
            RuntimeOverride::ExecutablePath(path) => {
                if !runtime_executable_ready_without_probe(&path) {
                    reasons.push(java_override_missing_reason());
                }
                return;
            }
            RuntimeOverride::Component(component) => {
                if !runtime_component_ready_without_probe(&request.library_dir, component.as_str())
                {
                    reasons.push(java_override_missing_reason());
                }
                return;
            }
            RuntimeOverride::None => {}
        }
    }

    let Some(version) = version else {
        return;
    };
    let component = preferred_runtime_component(&version.java_version);
    if !runtime_component_ready_without_probe(&request.library_dir, &component) {
        reasons.push(reason(
            LaunchReadinessReasonId::ManagedRuntimeMissing,
            "Managed Java runtime is missing and will be prepared before launch.",
            LaunchReadinessSeverity::Recoverable,
        ));
    }
}

fn reason_for_version_error(error: &LaunchModelError) -> LaunchReadinessReason {
    if is_missing_parent_version(error) {
        return reason(
            LaunchReadinessReasonId::ParentVersionMissing,
            "Parent version metadata is missing. Install the base version before launching.",
            LaunchReadinessSeverity::Blocking,
        );
    }

    reason(
        LaunchReadinessReasonId::VersionJsonMissing,
        "Installed version metadata is missing. Install this version before launching.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn is_missing_parent_version(error: &LaunchModelError) -> bool {
    match error {
        LaunchModelError::LoadParent { source, .. } => is_missing_version_json(source),
        _ => false,
    }
}

fn is_missing_version_json(error: &LaunchModelError) -> bool {
    match error {
        LaunchModelError::ReadVersion { source, .. } => source.kind() == ErrorKind::NotFound,
        LaunchModelError::LoadParent { source, .. } => is_missing_version_json(source),
        _ => false,
    }
}

fn java_override_missing_reason() -> LaunchReadinessReason {
    reason(
        LaunchReadinessReasonId::JavaOverrideMissing,
        "Selected Java override is unavailable. Choose another Java runtime or use Managed mode.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn reason(
    id: LaunchReadinessReasonId,
    message: &'static str,
    severity: LaunchReadinessSeverity,
) -> LaunchReadinessReason {
    LaunchReadinessReason {
        id,
        severity,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LaunchReadinessReasonId, LaunchReadinessRequest, LaunchReadinessSeverity,
        inspect_launch_readiness,
    };
    use crate::GuardianMode;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn managed_runtime_missing_is_recoverable_in_managed_mode() {
        let library_dir = temp_library("managed-runtime-missing-recoverable");
        write_version_json(
            &library_dir,
            "1.21.1",
            r#"{
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": {
                    "component": "croopor-test-runtime-missing",
                    "majorVersion": 21
                },
                "libraries": []
            }"#,
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            b"client jar",
        )
        .expect("write client jar");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(readiness.launchable, "{:?}", readiness.reasons);
        let reason = readiness
            .reasons
            .iter()
            .find(|reason| reason.id == LaunchReadinessReasonId::ManagedRuntimeMissing)
            .expect("managed runtime reason");
        assert_eq!(reason.severity, LaunchReadinessSeverity::Recoverable);
        cleanup(&library_dir);
    }

    #[test]
    fn custom_component_override_missing_stays_blocking() {
        let library_dir = temp_library("custom-runtime-missing-blocking");
        write_version_json(
            &library_dir,
            "1.21.1",
            r#"{
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": {
                    "component": "java-runtime-delta",
                    "majorVersion": 21
                },
                "libraries": []
            }"#,
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            b"client jar",
        )
        .expect("write client jar");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: "croopor-test-runtime-missing".to_string(),
            guardian_mode: GuardianMode::Custom,
        });

        assert!(!readiness.launchable);
        let reason = readiness
            .reasons
            .iter()
            .find(|reason| reason.id == LaunchReadinessReasonId::JavaOverrideMissing)
            .expect("custom override reason");
        assert_eq!(reason.severity, LaunchReadinessSeverity::Blocking);
        cleanup(&library_dir);
    }

    #[test]
    fn corrupt_client_jar_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-client-jar");
        let expected_client = b"fresh";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": []
                }}"#,
                sha1_hex(expected_client),
                expected_client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            b"wrong",
        )
        .expect("write corrupt client jar");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::ClientJarCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn corrupt_library_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-library");
        let client = b"client";
        let expected_library = b"fresh";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": [{{
                        "name": "com.example:demo:1.0.0",
                        "downloads": {{
                            "artifact": {{
                                "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                                "sha1": "{}",
                                "size": {}
                            }}
                        }}
                    }}]
                }}"#,
                sha1_hex(client),
                client.len(),
                sha1_hex(expected_library),
                expected_library.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            client,
        )
        .expect("write client jar");
        let library_path = library_dir
            .join("libraries")
            .join("com/example/demo/1.0.0/demo-1.0.0.jar");
        fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
        fs::write(&library_path, b"wrong").expect("write corrupt library");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn corrupt_asset_index_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-asset-index");
        let client = b"client";
        let expected_asset_index = b"fresh";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{
                        "id": "test-assets",
                        "sha1": "{}",
                        "size": {}
                    }},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": []
                }}"#,
                sha1_hex(expected_asset_index),
                expected_asset_index.len(),
                sha1_hex(client),
                client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            client,
        )
        .expect("write client jar");
        let asset_index_path = library_dir
            .join("assets")
            .join("indexes")
            .join("test-assets.json");
        fs::create_dir_all(asset_index_path.parent().expect("asset parent"))
            .expect("asset index dir");
        fs::write(&asset_index_path, b"wrong").expect("write corrupt asset index");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::AssetIndexCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    fn temp_library(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "croopor-launcher-readiness-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp library");
        root
    }

    fn write_version_json(library_dir: &Path, version_id: &str, json: &str) {
        let version_dir = library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(version_dir.join(format!("{version_id}.json")), json).expect("version json");
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
