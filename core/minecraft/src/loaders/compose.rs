use crate::download::ExpectedIntegrity;
use crate::launch::{
    ArgumentsSection, AssetIndex, Downloads, JavaVersion, Library, LoggingConf, VersionJson,
    merge_libraries_prefer_first, resolve_version,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::managed_fs::ManagedDir;
use super::types::LoaderError;
use super::validate_version_id;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoaderProfileFragment {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "inheritsFrom", default)]
    pub inherits_from: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "mainClass", default)]
    pub main_class: String,
    #[serde(rename = "minimumLauncherVersion", default)]
    pub minimum_launcher_version: i32,
    #[serde(rename = "complianceLevel", default)]
    pub compliance_level: i32,
    #[serde(rename = "releaseTime", default)]
    pub release_time: String,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub arguments: Option<ArgumentsSection>,
    #[serde(rename = "minecraftArguments", default)]
    pub minecraft_arguments: String,
    #[serde(rename = "assetIndex", default)]
    pub asset_index: Option<AssetIndex>,
    #[serde(default)]
    pub assets: String,
    #[serde(default)]
    pub downloads: Option<Downloads>,
    #[serde(rename = "javaVersion", default)]
    pub java_version: Option<JavaVersion>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub logging: Option<LoggingConf>,
}

pub fn compose_loader_version(
    base: &VersionJson,
    base_version_id: &str,
    version_id: &str,
    fragment: &LoaderProfileFragment,
) -> Result<VersionJson, LoaderError> {
    validate_version_id(base_version_id, "base minecraft version id")?;
    validate_version_id(version_id, "installed loader version id")?;
    if base.id != base_version_id || !base.inherits_from.is_empty() || base.materialized {
        return Err(LoaderError::InvalidProfile(
            "authenticated base identity does not match loader profile".to_string(),
        ));
    }

    let mut composed = VersionJson {
        id: version_id.to_string(),
        inherits_from: base_version_id.to_string(),
        materialized: true,
        kind: if fragment.kind.is_empty() {
            base.kind.clone()
        } else {
            fragment.kind.clone()
        },
        main_class: if fragment.main_class.is_empty() {
            base.main_class.clone()
        } else {
            fragment.main_class.clone()
        },
        minimum_launcher_version: if fragment.minimum_launcher_version != 0 {
            fragment.minimum_launcher_version
        } else {
            base.minimum_launcher_version
        },
        compliance_level: if fragment.compliance_level != 0 {
            fragment.compliance_level
        } else {
            base.compliance_level
        },
        release_time: if fragment.release_time.is_empty() {
            base.release_time.clone()
        } else {
            fragment.release_time.clone()
        },
        time: if fragment.time.is_empty() {
            base.time.clone()
        } else {
            fragment.time.clone()
        },
        arguments: merge_arguments(base.arguments.as_ref(), fragment.arguments.as_ref()),
        minecraft_arguments: if fragment.minecraft_arguments.is_empty() {
            base.minecraft_arguments.clone()
        } else {
            fragment.minecraft_arguments.clone()
        },
        asset_index: fragment
            .asset_index
            .clone()
            .unwrap_or_else(|| base.asset_index.clone()),
        assets: if fragment.assets.is_empty() {
            base.assets.clone()
        } else {
            fragment.assets.clone()
        },
        downloads: fragment
            .downloads
            .clone()
            .unwrap_or_else(|| base.downloads.clone()),
        java_version: fragment
            .java_version
            .clone()
            .unwrap_or_else(|| base.java_version.clone()),
        libraries: merge_libraries_prefer_first(&fragment.libraries, &base.libraries),
        logging: fragment.logging.clone().or_else(|| base.logging.clone()),
    };

    if composed.asset_index.id.is_empty() && !composed.assets.is_empty() {
        composed.asset_index.id = composed.assets.clone();
    }

    Ok(composed)
}

pub fn compose_loader_version_from_installed_base(
    mc_dir: &Path,
    base_version_id: &str,
    version_id: &str,
    fragment: &LoaderProfileFragment,
) -> Result<VersionJson, LoaderError> {
    let base = resolve_version(mc_dir, base_version_id).map_err(|error| {
        LoaderError::InstallExecutionFailed(format!("resolve base version: {error}"))
    })?;
    compose_loader_version(&base, base_version_id, version_id, fragment)
}

pub async fn write_composed_version(
    mc_dir: &Path,
    version_id: &str,
    version: &VersionJson,
    version_bytes: &[u8],
    base_version_id: &str,
    authenticated_client: &ExpectedIntegrity,
) -> Result<(), LoaderError> {
    validate_version_id(base_version_id, "base minecraft version id")?;
    validate_version_id(version_id, "installed loader version id")?;
    if version.id != version_id
        || version.inherits_from != base_version_id
        || !version.materialized
        || !serde_json::from_slice::<VersionJson>(version_bytes)
            .is_ok_and(|written| written == *version)
    {
        return Err(LoaderError::InvalidProfile(
            "composed loader profile identity does not match its install target".to_string(),
        ));
    }
    let root = ManagedDir::open_root(mc_dir)?;
    let versions = root.open_or_create_child("versions")?;
    let version_dir = versions.open_or_create_child(version_id)?;
    version_dir
        .write_exact(".incomplete", b"installing")
        .await?;
    version_dir
        .write_exact(&format!("{version_id}.json"), version_bytes)
        .await?;
    copy_authenticated_base_jar(
        &versions,
        &version_dir,
        base_version_id,
        version_id,
        authenticated_client,
    )
    .await?;
    root.revalidate()?;
    Ok(())
}

pub(crate) fn create_managed_version_dir(
    mc_dir: &Path,
    version_id: &str,
) -> Result<ManagedDir, LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    ManagedDir::open_root(mc_dir)?
        .open_or_create_child("versions")?
        .open_or_create_child(version_id)
}

pub(crate) fn managed_version_dir(
    mc_dir: &Path,
    version_id: &str,
) -> Result<ManagedDir, LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    ManagedDir::open_root(mc_dir)?
        .open_child("versions")?
        .open_child(version_id)
}

pub fn finalize_version_install(mc_dir: &Path, version_id: &str) -> Result<(), LoaderError> {
    let version_dir = managed_version_dir(mc_dir, version_id)?;
    if version_dir.read_exact(".incomplete")? != b"installing" {
        return Err(LoaderError::Verify(
            "managed loader completion marker is invalid".to_string(),
        ));
    }
    version_dir.remove_file(".incomplete")?;
    version_dir.revalidate()
}

pub fn cleanup_incomplete_version(mc_dir: &Path, version_id: &str) {
    if validate_version_id(version_id, "installed loader version id").is_err() {
        return;
    }
    let Ok(root) = ManagedDir::open_root(mc_dir) else {
        return;
    };
    let Ok(versions) = root.open_child("versions") else {
        return;
    };
    let Ok(version_dir) = versions.open_child(version_id) else {
        return;
    };
    if version_dir
        .read_exact(".incomplete")
        .is_ok_and(|marker| marker == b"installing")
    {
        let _ = version_dir.clear_owned_contents();
    }
}

fn merge_arguments(
    base: Option<&ArgumentsSection>,
    fragment: Option<&ArgumentsSection>,
) -> Option<ArgumentsSection> {
    if base.is_none() && fragment.is_none() {
        return None;
    }

    let mut merged = ArgumentsSection::default();
    if let Some(base) = base {
        merged.game.extend(base.game.clone());
        merged.jvm.extend(base.jvm.clone());
    }
    if let Some(fragment) = fragment {
        merged.game.extend(fragment.game.clone());
        merged.jvm.extend(fragment.jvm.clone());
    }
    Some(merged)
}

async fn copy_authenticated_base_jar(
    versions: &ManagedDir,
    destination: &ManagedDir,
    base_version_id: &str,
    version_id: &str,
    authenticated_client: &ExpectedIntegrity,
) -> Result<(), LoaderError> {
    validate_version_id(base_version_id, "base minecraft version id")?;
    validate_version_id(version_id, "installed loader version id")?;
    let base = versions.open_child(base_version_id)?;
    let bytes = base.read_authenticated(
        &format!("{base_version_id}.jar"),
        authenticated_client.size,
        authenticated_client.sha1.as_deref(),
    )?;
    destination
        .write_exact(&format!("{version_id}.jar"), &bytes)
        .await?;
    base.revalidate()?;
    versions.revalidate()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LoaderProfileFragment, cleanup_incomplete_version,
        compose_loader_version_from_installed_base, finalize_version_install, managed_version_dir,
        write_composed_version,
    };
    use crate::LoaderError;
    use crate::download::ExpectedIntegrity;
    use crate::launch::{AssetIndex, Downloads, JavaVersion, VersionJson, resolve_version};
    use crate::loaders::installed_metadata::INSTALLED_LOADER_METADATA_SCHEMA_VERSION;
    use crate::loaders::{LoaderComponentId, installed_version_id_for};
    use crate::paths::create_minecraft_dir;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn fragment_allows_missing_asset_index() {
        let json = r#"{
            "id":"fabric-loader-test-1.21.6",
            "inheritsFrom":"1.21.6",
            "mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient",
            "libraries":[{"name":"net.fabricmc:fabric-loader:0.16.10"}]
        }"#;
        let fragment = serde_json::from_str::<LoaderProfileFragment>(json).expect("fragment");
        assert!(fragment.asset_index.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn managed_version_directory_rejects_symlinked_versions_root() {
        let root = temp_dir("symlinked-versions-root");
        let outside = temp_dir("symlinked-versions-outside");
        fs::create_dir_all(&root).expect("minecraft root");
        fs::create_dir_all(&outside).expect("outside root");
        std::os::unix::fs::symlink(&outside, root.join("versions")).expect("symlink versions root");

        let error = managed_version_dir(&root, "managed-child")
            .expect_err("symlinked versions root must fail");

        assert!(matches!(error, LoaderError::Io(_) | LoaderError::Verify(_)));
        assert_eq!(fs::read_dir(&outside).expect("outside root").count(), 0);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn composed_version_inherits_asset_index_from_base() {
        let root = temp_dir("compose-loader-version");
        create_minecraft_dir(&root).expect("library");
        let version_dir = root.join("versions").join("1.21.6");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(
            version_dir.join("1.21.6.json"),
            r#"{
                "id":"1.21.6",
                "type":"release",
                "mainClass":"net.minecraft.client.main.Main",
                "arguments":{"game":[],"jvm":[]},
                "assetIndex":{"id":"1.21.6","url":"https://example.invalid/assets.json"},
                "downloads":{"client":{"url":"https://example.invalid/client.jar"}},
                "javaVersion":{"component":"java-runtime-gamma","majorVersion":21},
                "libraries":[]
            }"#,
        )
        .expect("base json");

        let fragment = serde_json::from_str::<LoaderProfileFragment>(
            r#"{
                "id":"fabric-loader-0.16.10-1.21.6",
                "inheritsFrom":"1.21.6",
                "mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient",
                "libraries":[{"name":"net.fabricmc:fabric-loader:0.16.10"}]
            }"#,
        )
        .expect("fragment");

        let version_id = installed_version_id_for(LoaderComponentId::Fabric, "1.21.6", "0.16.10")
            .expect("canonical installed id");
        let composed =
            compose_loader_version_from_installed_base(&root, "1.21.6", &version_id, &fragment)
                .expect("compose");
        assert_eq!(composed.asset_index.id, "1.21.6");
        assert_eq!(
            composed.main_class,
            "net.fabricmc.loader.impl.launch.knot.KnotClient"
        );
        assert!(
            composed
                .libraries
                .iter()
                .any(|library| library.name == "net.fabricmc:fabric-loader:0.16.10")
        );
        assert_eq!(
            composed
                .libraries
                .iter()
                .filter(|library| library.name.starts_with("org.ow2.asm:asm:"))
                .count(),
            0
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn composed_version_prefers_loader_library_over_base_duplicate() {
        let root = temp_dir("compose-loader-version-dedup");
        create_minecraft_dir(&root).expect("library");
        let version_dir = root.join("versions").join("1.21.6");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(
            version_dir.join("1.21.6.json"),
            r#"{
                "id":"1.21.6",
                "type":"release",
                "mainClass":"net.minecraft.client.main.Main",
                "arguments":{"game":[],"jvm":[]},
                "assetIndex":{"id":"1.21.6","url":"https://example.invalid/assets.json"},
                "downloads":{"client":{"url":"https://example.invalid/client.jar"}},
                "javaVersion":{"component":"java-runtime-gamma","majorVersion":21},
                "libraries":[{"name":"org.ow2.asm:asm:9.6"}]
            }"#,
        )
        .expect("base json");

        let fragment = serde_json::from_str::<LoaderProfileFragment>(
            r#"{
                "id":"fabric-loader-0.16.10-1.21.6",
                "inheritsFrom":"1.21.6",
                "mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient",
                "libraries":[
                    {"name":"net.fabricmc:fabric-loader:0.16.10"},
                    {"name":"org.ow2.asm:asm:9.9"}
                ]
            }"#,
        )
        .expect("fragment");

        let version_id = installed_version_id_for(LoaderComponentId::Fabric, "1.21.6", "0.16.10")
            .expect("canonical installed id");
        let composed =
            compose_loader_version_from_installed_base(&root, "1.21.6", &version_id, &fragment)
                .expect("compose");

        let asm_libraries = composed
            .libraries
            .iter()
            .filter(|library| library.name.starts_with("org.ow2.asm:asm:"))
            .map(|library| library.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(asm_libraries, vec!["org.ow2.asm:asm:9.9".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_incomplete_version_ignores_empty_version_id() {
        let root = temp_dir("cleanup-empty-version-id");
        create_minecraft_dir(&root).expect("library");
        let retained = root.join("versions").join("retained");
        fs::create_dir_all(&retained).expect("retained version");

        cleanup_incomplete_version(&root, "   ");

        assert!(root.join("versions").is_dir());
        assert!(retained.is_dir());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_incomplete_version_ignores_traversal_version_id() {
        let root = temp_dir("cleanup-traversal-version-id");
        create_minecraft_dir(&root).expect("library");
        let retained = root.join("versions").join("retained");
        fs::create_dir_all(&retained).expect("retained version");

        cleanup_incomplete_version(&root, "..");

        assert!(root.join("versions").is_dir());
        assert!(retained.is_dir());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_incomplete_version_preserves_complete_version_without_marker() {
        let root = temp_dir("cleanup-complete-version");
        create_minecraft_dir(&root).expect("library");
        let version_dir = root.join("versions").join("loader-complete");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(
            version_dir.join("loader-complete.json"),
            br#"{"id":"loader-complete"}"#,
        )
        .expect("version json");

        cleanup_incomplete_version(&root, "loader-complete");

        assert!(version_dir.is_dir());
        assert!(version_dir.join("loader-complete.json").is_file());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_incomplete_version_retains_only_cleared_admitted_shell() {
        let root = temp_dir("cleanup-incomplete-retained-shell");
        create_minecraft_dir(&root).expect("library");
        let version_dir = root.join("versions").join("loader-incomplete");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(version_dir.join(".incomplete"), b"installing").expect("marker");
        fs::write(version_dir.join("loader-incomplete.json"), b"partial").expect("partial file");

        cleanup_incomplete_version(&root, "loader-incomplete");

        assert!(version_dir.is_dir());
        assert_eq!(
            fs::read_dir(&version_dir)
                .expect("cleared version shell")
                .count(),
            0
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn finalize_version_install_rejects_missing_version_directory() {
        let root = temp_dir("finalize-missing-version");
        create_minecraft_dir(&root).expect("library");

        let error = finalize_version_install(&root, "loader-missing")
            .expect_err("missing version directory must not be created");

        assert!(matches!(error, LoaderError::Io(_) | LoaderError::Verify(_)));
        assert!(!root.join("versions").join("loader-missing").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn finalize_version_install_requires_expected_completion_marker() {
        let root = temp_dir("finalize-marker");
        create_minecraft_dir(&root).expect("library");
        let version_dir = root.join("versions").join("loader-marker");
        fs::create_dir_all(&version_dir).expect("version dir");

        let missing =
            finalize_version_install(&root, "loader-marker").expect_err("missing marker must fail");
        assert!(matches!(
            missing,
            LoaderError::Io(_) | LoaderError::Verify(_)
        ));

        let marker = version_dir.join(".incomplete");
        fs::write(&marker, b"unexpected").expect("invalid marker");
        let invalid =
            finalize_version_install(&root, "loader-marker").expect_err("invalid marker must fail");
        assert!(matches!(invalid, LoaderError::Verify(_)));
        assert_eq!(fs::read(&marker).expect("retained marker"), b"unexpected");

        fs::write(&marker, b"installing").expect("valid marker");
        finalize_version_install(&root, "loader-marker").expect("finalize version");
        assert!(!marker.exists());
        assert!(version_dir.is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn write_composed_version_rejects_traversal_base_version_id() {
        let root = temp_dir("write-composed-version-base-traversal");
        create_minecraft_dir(&root).expect("library");
        let version = VersionJson {
            id: "loader-test".to_string(),
            inherits_from: String::new(),
            materialized: false,
            kind: String::new(),
            main_class: String::new(),
            minimum_launcher_version: 0,
            compliance_level: 0,
            release_time: String::new(),
            time: String::new(),
            arguments: None,
            minecraft_arguments: String::new(),
            asset_index: AssetIndex::default(),
            assets: String::new(),
            downloads: Downloads::default(),
            java_version: JavaVersion::default(),
            libraries: Vec::new(),
            logging: None,
        };

        let error = write_composed_version(
            &root,
            "loader-test",
            &version,
            &serde_json::to_vec_pretty(&version).expect("serialize version"),
            "../escape",
            &ExpectedIntegrity::default(),
        )
        .await
        .expect_err("traversal should fail");

        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "base minecraft version id contains path separators"
        ));
        assert!(!root.join("versions").join("loader-test").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn write_composed_version_declares_parent_without_remerging_it() {
        let root = temp_dir("write-composed-version-standalone");
        create_minecraft_dir(&root).expect("library");
        let base_dir = root.join("versions").join("1.21.6");
        fs::create_dir_all(&base_dir).expect("base version dir");
        fs::write(base_dir.join("1.21.6.jar"), b"base jar").expect("base jar");
        fs::write(
            base_dir.join("1.21.6.json"),
            r#"{
                "id":"1.21.6",
                "type":"release",
                "mainClass":"net.minecraft.client.main.Main",
                "arguments":{
                    "game":["--username","${auth_player_name}"],
                    "jvm":["-cp","${classpath}"]
                },
                "assetIndex":{"id":"1.21.6","url":"https://example.invalid/assets.json"},
                "downloads":{"client":{"url":"https://example.invalid/client.jar"}},
                "javaVersion":{"component":"java-runtime-gamma","majorVersion":21},
                "libraries":[{"name":"com.mojang:base:1.0"}]
            }"#,
        )
        .expect("base json");

        let fragment = serde_json::from_str::<LoaderProfileFragment>(
            r#"{
                "id":"fabric-loader-0.16.10-1.21.6",
                "inheritsFrom":"1.21.6",
                "mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient",
                "arguments":{
                    "game":["--fabric.gameJarPath","${primary_jar}"],
                    "jvm":["-Dfabric.side=client"]
                },
                "libraries":[{"name":"net.fabricmc:fabric-loader:0.16.10"}]
            }"#,
        )
        .expect("fragment");

        let version_id = installed_version_id_for(LoaderComponentId::Fabric, "1.21.6", "0.16.10")
            .expect("canonical installed version id");
        let composed =
            compose_loader_version_from_installed_base(&root, "1.21.6", &version_id, &fragment)
                .expect("compose loader version");
        write_composed_version(
            &root,
            &version_id,
            &composed,
            &serde_json::to_vec_pretty(&composed).expect("serialize composed version"),
            "1.21.6",
            &expected_integrity(b"base jar"),
        )
        .await
        .expect("write composed version");
        fs::write(
            root.join("versions")
                .join(&version_id)
                .join(".axial-loader.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": INSTALLED_LOADER_METADATA_SCHEMA_VERSION,
                "component_id": LoaderComponentId::Fabric,
                "minecraft_version": "1.21.6",
                "loader_version": "0.16.10"
            }))
            .expect("serialize installed loader metadata"),
        )
        .expect("write installed loader metadata");

        let written_json = fs::read_to_string(
            root.join("versions")
                .join(&version_id)
                .join(format!("{version_id}.json")),
        )
        .expect("read written json");
        let written: serde_json::Value =
            serde_json::from_str(&written_json).expect("parse written json");
        assert_eq!(written["inheritsFrom"], "1.21.6");
        assert_eq!(written["axialMaterialized"], true);

        let resolved = resolve_version(&root, &version_id).expect("resolve written loader version");
        assert!(resolved.inherits_from.is_empty());
        let arguments = resolved.arguments.expect("resolved arguments");
        assert_eq!(count_arg_value(&arguments.jvm, "-cp"), 1);
        assert_eq!(count_arg_value(&arguments.jvm, "-Dfabric.side=client"), 1);
        assert_eq!(count_arg_value(&arguments.game, "--username"), 1);
        assert_eq!(count_arg_value(&arguments.game, "--fabric.gameJarPath"), 1);
        assert_eq!(count_library(&resolved.libraries, "com.mojang:base:1.0"), 1);
        assert_eq!(
            count_library(&resolved.libraries, "net.fabricmc:fabric-loader:0.16.10"),
            1
        );

        let _ = fs::remove_dir_all(root);
    }

    fn expected_integrity(bytes: &[u8]) -> ExpectedIntegrity {
        use sha1::{Digest as _, Sha1};

        ExpectedIntegrity {
            size: Some(bytes.len() as u64),
            sha1: Some(format!("{:x}", Sha1::digest(bytes))),
        }
    }

    fn count_arg_value(values: &[crate::launch::Argument], needle: &str) -> usize {
        values
            .iter()
            .flat_map(|argument| argument.value.iter())
            .filter(|value| value.as_str() == needle)
            .count()
    }

    fn count_library(libraries: &[crate::launch::Library], name: &str) -> usize {
        libraries
            .iter()
            .filter(|library| library.name == name)
            .count()
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }
}
