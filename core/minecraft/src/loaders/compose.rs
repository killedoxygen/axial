use crate::download::{
    ExpectedIntegrity, LauncherManagedArtifactReadiness,
    promote_launcher_managed_artifact_temp_once, verify_existing_launcher_managed_artifact,
};
use crate::launch::{
    ArgumentsSection, AssetIndex, Downloads, JavaVersion, Library, LoggingConf, VersionJson,
    merge_libraries_prefer_first, resolve_version,
};
use crate::paths::versions_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;

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
    let version_dir = prepare_managed_version_dir(mc_dir, version_id)?;
    let marker = version_dir.join(".incomplete");
    write_exact_managed_version_artifact(&marker, b"installing").await?;
    write_exact_managed_version_artifact(
        &version_dir.join(format!("{version_id}.json")),
        version_bytes,
    )
    .await?;
    link_or_copy_base_jar(mc_dir, base_version_id, version_id, authenticated_client).await?;
    Ok(())
}

pub(crate) async fn write_exact_managed_version_artifact(
    path: &Path,
    source_bytes: &[u8],
) -> Result<(), LoaderError> {
    let parent = path.parent().ok_or_else(|| {
        LoaderError::Verify("managed loader artifact has no version directory".to_string())
    })?;
    require_exact_directory(parent, "managed version directory")?;
    let temp_path = managed_artifact_temp_path(path);
    let mut output = async_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await?;
    if let Err(error) = async {
        output.write_all(source_bytes).await?;
        output.flush().await?;
        output.sync_all().await
    }
    .await
    {
        drop(output);
        let _ = async_fs::remove_file(&temp_path).await;
        return Err(LoaderError::Io(error));
    }
    drop(output);
    if async_fs::read(&temp_path).await? != source_bytes {
        let _ = async_fs::remove_file(&temp_path).await;
        return Err(LoaderError::Verify(
            "temporary loader artifact differs from authenticated source bytes".to_string(),
        ));
    }
    if let Err(error) = promote_launcher_managed_artifact_temp_once(&temp_path, path).await {
        let _ = async_fs::remove_file(&temp_path).await;
        return Err(LoaderError::Io(error));
    }
    if async_fs::read(path).await? != source_bytes {
        let _ = async_fs::remove_file(path).await;
        return Err(LoaderError::Verify(
            "installed loader artifact differs from authenticated source bytes".to_string(),
        ));
    }
    Ok(())
}

fn managed_artifact_temp_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!("tmp-{}-{nanos:x}", std::process::id()))
}

pub(crate) fn prepare_managed_version_dir(
    mc_dir: &Path,
    version_id: &str,
) -> Result<PathBuf, LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    require_exact_directory(mc_dir, "minecraft root")?;
    let versions = versions_dir(mc_dir);
    create_exact_directory_if_missing(&versions, "versions root")?;
    let version_dir = versions.join(version_id);
    create_exact_directory_if_missing(&version_dir, "managed version directory")?;
    Ok(version_dir)
}

fn create_exact_directory_if_missing(path: &Path, label: &str) -> Result<(), LoaderError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => require_directory_metadata(metadata, label),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            require_exact_directory(path, label)
        }
        Err(error) => Err(LoaderError::Io(error)),
    }
}

fn require_exact_directory(path: &Path, label: &str) -> Result<(), LoaderError> {
    let metadata = fs::symlink_metadata(path)?;
    require_directory_metadata(metadata, label)
}

fn require_directory_metadata(metadata: fs::Metadata, label: &str) -> Result<(), LoaderError> {
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LoaderError::Verify(format!(
            "{label} is not an exact managed directory"
        )))
    }
}

pub fn finalize_version_install(mc_dir: &Path, version_id: &str) -> Result<(), LoaderError> {
    let version_dir = prepare_managed_version_dir(mc_dir, version_id)?;
    let marker = version_dir.join(".incomplete");
    if marker.exists() {
        let _ = fs::remove_file(marker);
    }
    Ok(())
}

pub fn cleanup_incomplete_version(mc_dir: &Path, version_id: &str) {
    if validate_version_id(version_id, "installed loader version id").is_err() {
        return;
    }
    let version_dir = versions_dir(mc_dir).join(version_id);
    let marker = version_dir.join(".incomplete");
    let marker_is_regular_file = fs::symlink_metadata(&marker)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false);
    if marker_is_regular_file {
        let _ = fs::remove_dir_all(version_dir);
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

async fn link_or_copy_base_jar(
    mc_dir: &Path,
    base_version_id: &str,
    version_id: &str,
    authenticated_client: &ExpectedIntegrity,
) -> Result<(), LoaderError> {
    validate_version_id(base_version_id, "base minecraft version id")?;
    validate_version_id(version_id, "installed loader version id")?;
    let base_jar = versions_dir(mc_dir)
        .join(base_version_id)
        .join(format!("{base_version_id}.jar"));
    if verify_existing_launcher_managed_artifact(&base_jar, authenticated_client)
        != LauncherManagedArtifactReadiness::Verified
    {
        return Err(LoaderError::Verify(format!(
            "base client jar is not authenticated for {base_version_id}"
        )));
    }
    let dst_jar = versions_dir(mc_dir)
        .join(version_id)
        .join(format!("{version_id}.jar"));
    if verify_existing_launcher_managed_artifact(&dst_jar, authenticated_client)
        == LauncherManagedArtifactReadiness::Verified
    {
        return Ok(());
    }
    if async_fs::symlink_metadata(&dst_jar).await.is_ok() {
        async_fs::remove_file(&dst_jar).await?;
    }
    if async_fs::hard_link(&base_jar, &dst_jar).await.is_err() {
        let temp_jar = dst_jar.with_extension("jar.axial-tmp");
        if async_fs::symlink_metadata(&temp_jar).await.is_ok() {
            async_fs::remove_file(&temp_jar).await?;
        }
        async_fs::copy(&base_jar, &temp_jar).await?;
        if verify_existing_launcher_managed_artifact(&temp_jar, authenticated_client)
            != LauncherManagedArtifactReadiness::Verified
        {
            let _ = async_fs::remove_file(&temp_jar).await;
            return Err(LoaderError::Verify(format!(
                "copied client jar is not authenticated for {version_id}"
            )));
        }
        async_fs::rename(&temp_jar, &dst_jar).await?;
    }
    if verify_existing_launcher_managed_artifact(&dst_jar, authenticated_client)
        != LauncherManagedArtifactReadiness::Verified
    {
        let _ = async_fs::remove_file(&dst_jar).await;
        return Err(LoaderError::Verify(format!(
            "installed client jar is not authenticated for {version_id}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LoaderProfileFragment, cleanup_incomplete_version,
        compose_loader_version_from_installed_base, prepare_managed_version_dir,
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

        let error = prepare_managed_version_dir(&root, "managed-child")
            .expect_err("symlinked versions root must fail");

        assert!(
            matches!(error, LoaderError::Verify(message) if message.contains("exact managed directory"))
        );
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
