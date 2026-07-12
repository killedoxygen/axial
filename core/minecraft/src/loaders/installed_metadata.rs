use super::api::installed_version_id_for;
use super::providers::common::infer_loader_build_metadata;
use super::types::{LoaderBuildMetadata, LoaderBuildRecord, LoaderComponentId};
use crate::launch::{VersionJson, load_version_json};
use crate::paths::versions_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub(crate) const INSTALLED_LOADER_METADATA_SCHEMA_VERSION: u32 = 2;
const MAX_INSTALLED_LOADER_METADATA_BYTES: u64 = 4 << 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstalledLoaderMetadata {
    pub(crate) schema_version: u32,
    pub(crate) component_id: LoaderComponentId,
    pub(crate) minecraft_version: String,
    pub(crate) loader_version: String,
}

impl From<&LoaderBuildRecord> for InstalledLoaderMetadata {
    fn from(record: &LoaderBuildRecord) -> Self {
        Self {
            schema_version: INSTALLED_LOADER_METADATA_SCHEMA_VERSION,
            component_id: record.component_id,
            minecraft_version: record.minecraft_version.clone(),
            loader_version: record.loader_version.clone(),
        }
    }
}

impl InstalledLoaderMetadata {
    pub(crate) fn is_valid_for_profile(
        &self,
        installed_version_id: &str,
        profile_id: &str,
        declared_parent: &str,
        materialized: bool,
    ) -> bool {
        self.schema_version == INSTALLED_LOADER_METADATA_SCHEMA_VERSION
            && materialized
            && profile_id == installed_version_id
            && declared_parent == self.minecraft_version
            && !self.minecraft_version.is_empty()
            && self.minecraft_version == self.minecraft_version.trim()
            && !self.loader_version.is_empty()
            && self.loader_version == self.loader_version.trim()
            && installed_version_id_for(
                self.component_id,
                &self.minecraft_version,
                &self.loader_version,
            )
            .is_ok_and(|expected| expected == installed_version_id)
    }

    pub(crate) fn display_metadata(&self) -> LoaderBuildMetadata {
        infer_loader_build_metadata(&self.loader_version, &[], false, false, None)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledLoaderProvenance {
    component_id: LoaderComponentId,
    minecraft_version: String,
    loader_version: String,
}

impl InstalledLoaderProvenance {
    pub fn component_id(&self) -> LoaderComponentId {
        self.component_id
    }

    pub fn minecraft_version(&self) -> &str {
        &self.minecraft_version
    }

    pub fn loader_version(&self) -> &str {
        &self.loader_version
    }
}

pub fn validated_installed_loader_provenance(
    mc_dir: &Path,
    installed_version_id: &str,
) -> Option<InstalledLoaderProvenance> {
    super::validate_version_id(installed_version_id, "installed loader version id").ok()?;
    let profile = load_version_json(mc_dir, installed_version_id).ok()?;
    read_validated_metadata_for_profile(mc_dir, installed_version_id, &profile).map(|metadata| {
        InstalledLoaderProvenance {
            component_id: metadata.component_id,
            minecraft_version: metadata.minecraft_version,
            loader_version: metadata.loader_version,
        }
    })
}

pub(crate) fn materialized_profile_has_valid_provenance(
    mc_dir: &Path,
    installed_version_id: &str,
    profile: &VersionJson,
) -> bool {
    read_validated_metadata_for_profile(mc_dir, installed_version_id, profile).is_some()
}

fn read_validated_metadata_for_profile(
    mc_dir: &Path,
    installed_version_id: &str,
    profile: &VersionJson,
) -> Option<InstalledLoaderMetadata> {
    let path = versions_dir(mc_dir)
        .join(installed_version_id)
        .join(".axial-loader.json");
    let file_metadata = fs::symlink_metadata(&path).ok()?;
    if !file_metadata.file_type().is_file()
        || file_metadata.file_type().is_symlink()
        || file_metadata.len() > MAX_INSTALLED_LOADER_METADATA_BYTES
    {
        return None;
    }
    let data = fs::read(path).ok()?;
    if data.len() as u64 > MAX_INSTALLED_LOADER_METADATA_BYTES {
        return None;
    }
    let metadata = serde_json::from_slice::<InstalledLoaderMetadata>(&data).ok()?;
    metadata
        .is_valid_for_profile(
            installed_version_id,
            &profile.id,
            &profile.inherits_from,
            profile.materialized,
        )
        .then_some(metadata)
}

pub(crate) fn installed_loader_metadata_bytes(
    record: &LoaderBuildRecord,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec_pretty(&InstalledLoaderMetadata::from(record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{LaunchModelError, resolve_version};
    use crate::loaders::types::{LoaderSelectionReason, LoaderTerm};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn previous_presentation_bearing_shape_is_rejected() {
        let previous = br#"{
            "schema_version":1,
            "component_id":"net.fabricmc.fabric-loader",
            "component_name":"Fabric",
            "minecraft_version":"1.21.5",
            "loader_version":"0.16.10",
            "build_meta":{}
        }"#;

        assert!(serde_json::from_slice::<InstalledLoaderMetadata>(previous).is_err());
    }

    #[test]
    fn immutable_provenance_derives_display_metadata_locally() {
        let metadata = InstalledLoaderMetadata {
            schema_version: INSTALLED_LOADER_METADATA_SCHEMA_VERSION,
            component_id: LoaderComponentId::Forge,
            minecraft_version: "1.20.1".to_string(),
            loader_version: "47.4.0-beta".to_string(),
        };

        let version_id =
            installed_version_id_for(LoaderComponentId::Forge, "1.20.1", "47.4.0-beta")
                .expect("valid loader identity");
        assert!(metadata.is_valid_for_profile(&version_id, &version_id, "1.20.1", true));
        let display = metadata.display_metadata();
        assert_eq!(display.terms, vec![LoaderTerm::Beta]);
        assert_eq!(display.display_tags, vec!["beta"]);
        assert_eq!(display.selection.reason, LoaderSelectionReason::Unstable);
    }

    #[test]
    fn current_sidecar_authorizes_exact_materialized_profile() {
        let root = provenance_fixture("valid", true, "1.21.1");
        let version_id = fabric_version_id();

        let provenance =
            validated_installed_loader_provenance(&root, &version_id).expect("valid provenance");
        assert_eq!(provenance.component_id(), LoaderComponentId::Fabric);
        assert_eq!(provenance.minecraft_version(), "1.21.1");
        assert!(resolve_version(&root, &version_id).is_ok());

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn missing_sidecar_cannot_authorize_materialized_profile() {
        let root = provenance_fixture("missing", true, "1.21.1");
        let version_id = fabric_version_id();
        fs::remove_file(
            versions_dir(&root)
                .join(&version_id)
                .join(".axial-loader.json"),
        )
        .expect("remove sidecar");

        assert_materialized_provenance_rejected(&root, &version_id);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn old_sidecar_cannot_authorize_materialized_profile() {
        let root = provenance_fixture("old", true, "1.21.1");
        let version_id = fabric_version_id();
        fs::write(
            versions_dir(&root)
                .join(&version_id)
                .join(".axial-loader.json"),
            r#"{
                "schema_version": 1,
                "component_id": "net.fabricmc.fabric-loader",
                "component_name": "Fabric",
                "minecraft_version": "1.21.1",
                "loader_version": "0.16.10"
            }"#,
        )
        .expect("write old sidecar");

        assert_materialized_provenance_rejected(&root, &version_id);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn forged_sidecar_cannot_authorize_materialized_profile() {
        let root = provenance_fixture("forged", true, "1.21.1");
        let version_id = fabric_version_id();
        write_sidecar(&root, &version_id, "1.21.2");

        assert_materialized_provenance_rejected(&root, &version_id);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn false_materialized_marker_cannot_authorize_sidecar() {
        let root = provenance_fixture("false-marker", false, "1.21.1");
        let version_id = fabric_version_id();

        assert!(validated_installed_loader_provenance(&root, &version_id).is_none());
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn oversized_sidecar_cannot_authorize_materialized_profile() {
        let root = provenance_fixture("oversized", true, "1.21.1");
        let version_id = fabric_version_id();
        fs::write(
            versions_dir(&root)
                .join(&version_id)
                .join(".axial-loader.json"),
            vec![b' '; MAX_INSTALLED_LOADER_METADATA_BYTES as usize + 1],
        )
        .expect("write oversized sidecar");

        assert_materialized_provenance_rejected(&root, &version_id);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_sidecar_cannot_authorize_materialized_profile() {
        use std::os::unix::fs::symlink;

        let root = provenance_fixture("symlink", true, "1.21.1");
        let version_id = fabric_version_id();
        let version_dir = versions_dir(&root).join(&version_id);
        let sidecar = version_dir.join(".axial-loader.json");
        let target = version_dir.join("metadata-target.json");
        fs::rename(&sidecar, &target).expect("move sidecar target");
        symlink(&target, &sidecar).expect("create sidecar symlink");

        assert_materialized_provenance_rejected(&root, &version_id);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    fn provenance_fixture(name: &str, materialized: bool, parent: &str) -> PathBuf {
        let root = unique_test_dir(name);
        let version_id = fabric_version_id();
        let version_dir = versions_dir(&root).join(&version_id);
        fs::create_dir_all(&version_dir).expect("create version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": &version_id,
                "inheritsFrom": parent,
                "axialMaterialized": materialized,
                "type": "release",
                "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "assetIndex": {},
                "libraries": []
            }))
            .expect("serialize profile"),
        )
        .expect("write profile");
        write_sidecar(&root, &version_id, "1.21.1");
        root
    }

    fn write_sidecar(root: &Path, version_id: &str, minecraft_version: &str) {
        fs::write(
            versions_dir(root)
                .join(version_id)
                .join(".axial-loader.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": INSTALLED_LOADER_METADATA_SCHEMA_VERSION,
                "component_id": LoaderComponentId::Fabric,
                "minecraft_version": minecraft_version,
                "loader_version": "0.16.10"
            }))
            .expect("serialize sidecar"),
        )
        .expect("write sidecar");
    }

    fn assert_materialized_provenance_rejected(root: &Path, version_id: &str) {
        assert!(validated_installed_loader_provenance(root, version_id).is_none());
        assert!(matches!(
            resolve_version(root, version_id),
            Err(LaunchModelError::InvalidMaterializedProvenance { .. })
        ));
    }

    fn fabric_version_id() -> String {
        installed_version_id_for(LoaderComponentId::Fabric, "1.21.1", "0.16.10")
            .expect("valid loader identity")
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("axial-loader-provenance-{name}-{unique}"))
    }
}
