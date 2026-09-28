use crate::paths::assets_dir;
use serde::Deserialize;
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AssetIndexFlagsError {
    #[error("failed to read asset index: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse asset index flags: {0}")]
    Parse(#[from] serde_json::Error),
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AssetIndexFlags {
    #[serde(default, rename = "virtual")]
    virtual_flag: bool,
    #[serde(default, rename = "map_to_resources")]
    map_to_resources: bool,
}

impl AssetIndexFlags {
    pub(crate) fn requires_virtual_repair(&self) -> bool {
        self.virtual_flag || self.map_to_resources
    }
}

pub fn asset_index_requires_virtual_repair(
    mc_dir: &Path,
    asset_index_id: &str,
) -> Result<bool, AssetIndexFlagsError> {
    let asset_index_id = asset_index_id.trim();
    if asset_index_id.is_empty() {
        return Ok(false);
    }

    let index_path = assets_dir(mc_dir)
        .join("indexes")
        .join(format!("{asset_index_id}.json"));
    let data = fs::read_to_string(index_path)?;
    let flags = serde_json::from_str::<AssetIndexFlags>(&data)?;
    Ok(flags.requires_virtual_repair())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn lightweight_parse_ignores_asset_objects_for_modern_index() {
        let root = temp_dir("modern-flags");
        write_index(
            &root,
            "modern",
            r#"{"objects":"projection-does-not-materialize-this"}"#,
        );

        assert!(!asset_index_requires_virtual_repair(&root, "modern").expect("modern flags"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn either_legacy_flag_requires_virtual_repair() {
        let root = temp_dir("legacy-flags");
        write_index(&root, "virtual", r#"{"objects":{},"virtual":true}"#);
        write_index(
            &root,
            "resources",
            r#"{"objects":{},"map_to_resources":true}"#,
        );

        assert!(asset_index_requires_virtual_repair(&root, "virtual").expect("virtual flag"));
        assert!(asset_index_requires_virtual_repair(&root, "resources").expect("resources flag"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_present_index_fails_instead_of_becoming_modern() {
        let root = temp_dir("malformed-flags");
        write_index(&root, "malformed", r#"{"virtual":"yes"}"#);

        assert!(matches!(
            asset_index_requires_virtual_repair(&root, "malformed"),
            Err(AssetIndexFlagsError::Parse(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_nonempty_index_fails_closed() {
        let root = temp_dir("missing-index");

        assert!(matches!(
            asset_index_requires_virtual_repair(&root, "missing"),
            Err(AssetIndexFlagsError::Read(error))
                if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    fn write_index(root: &Path, id: &str, contents: &str) {
        let indexes_dir = assets_dir(root).join("indexes");
        fs::create_dir_all(&indexes_dir).expect("asset indexes directory");
        fs::write(indexes_dir.join(format!("{id}.json")), contents).expect("asset index");
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-asset-index-{label}-{}-{nanos}",
            std::process::id()
        ))
    }
}
