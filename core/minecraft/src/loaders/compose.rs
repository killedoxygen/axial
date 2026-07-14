use crate::launch::{
    ArgumentsSection, AssetIndex, Downloads, JavaVersion, Library, LoggingConf, VersionJson,
    merge_libraries_prefer_first,
};
use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::{LoaderProfileFragment, compose_loader_version};
    use crate::launch::resolve_version;
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
        let base = resolve_version(&root, "1.21.6").expect("resolve base");
        let composed =
            compose_loader_version(&base, "1.21.6", &version_id, &fragment).expect("compose");
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
        let base = resolve_version(&root, "1.21.6").expect("resolve base");
        let composed =
            compose_loader_version(&base, "1.21.6", &version_id, &fragment).expect("compose");

        let asm_libraries = composed
            .libraries
            .iter()
            .filter(|library| library.name.starts_with("org.ow2.asm:asm:"))
            .map(|library| library.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(asm_libraries, vec!["org.ow2.asm:asm:9.9".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }
}
