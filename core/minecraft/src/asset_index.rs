//! Asset index format: hashed objects and the legacy trees named by the index.

use crate::download::decode_sha1;
use crate::known_good::{MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES, MAX_TIER2_ENTRIES};
use crate::portable_path::PortableRelativePath;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Deserialize)]
pub(crate) struct AssetIndex {
    pub(crate) objects: HashMap<String, AssetObject>,
    #[serde(default, rename = "virtual")]
    virtual_tree: bool,
    #[serde(default)]
    map_to_resources: bool,
}

#[derive(Deserialize)]
pub(crate) struct AssetObject {
    pub(crate) hash: String,
    pub(crate) size: i64,
}

pub(crate) fn parse_asset_index(bytes: &[u8]) -> Result<AssetIndex, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Where old clients read an index's objects by name instead of by hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyAssetLayout {
    /// `assets/virtual/<index id>` in the library, passed as `--assetsDir`.
    Virtual,
    /// `resources` in the game directory, which pre-1.6 clients read directly.
    Resources,
}

#[derive(Debug)]
pub struct LegacyAssetFile {
    pub path: PortableRelativePath,
    /// Lowercase hexadecimal SHA-1, naming `objects/<first two>/<hash>`.
    pub hash: String,
    pub sha1: [u8; 20],
    pub size: u64,
}

impl LegacyAssetFile {
    pub fn object_path(&self) -> String {
        format!("objects/{}/{}", &self.hash[..2], self.hash)
    }
}

#[derive(Debug)]
pub struct LegacyAssets {
    pub layout: LegacyAssetLayout,
    /// Sorted by path; no two paths alias under portable comparison.
    pub files: Vec<LegacyAssetFile>,
}

#[derive(Debug, Error)]
pub enum AssetIndexError {
    #[error("asset index is malformed: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(&'static str),
}

/// Projects an index onto the named tree its legacy flags require, or `None`
/// for indexes read only from the hashed object store. As in Mojang's launcher,
/// `map_to_resources` selects the game directory even when `virtual` is also set.
pub fn legacy_assets(index: &[u8]) -> Result<Option<LegacyAssets>, AssetIndexError> {
    use AssetIndexError::Invalid;

    let index = parse_asset_index(index)?;
    let layout = match (index.map_to_resources, index.virtual_tree) {
        (true, _) => LegacyAssetLayout::Resources,
        (false, true) => LegacyAssetLayout::Virtual,
        (false, false) => return Ok(None),
    };
    if index.objects.len() > MAX_TIER2_ENTRIES {
        return Err(Invalid("asset index exceeds the entry bound"));
    }
    let mut paths = HashSet::with_capacity(index.objects.len());
    let mut sizes = HashMap::<String, u64>::with_capacity(index.objects.len());
    let mut total = 0_u64;
    let mut files = Vec::with_capacity(index.objects.len());
    for (name, object) in index.objects {
        let path = PortableRelativePath::new_exact(&name)
            .map_err(|_| Invalid("legacy asset path is unsafe"))?;
        if !paths.insert(path.key()) {
            return Err(Invalid(
                "legacy asset paths contain a portable alias collision",
            ));
        }
        let hash = object.hash.to_ascii_lowercase();
        let sha1 = decode_sha1(&hash).ok_or(Invalid("asset object digest is invalid"))?;
        let size = u64::try_from(object.size)
            .ok()
            .filter(|size| *size <= MAX_TIER2_ARTIFACT_BYTES)
            .ok_or(Invalid("asset object size is invalid"))?;
        if sizes
            .insert(hash.clone(), size)
            .is_some_and(|previous| previous != size)
        {
            return Err(Invalid("asset object digest has conflicting sizes"));
        }
        total = total
            .checked_add(size)
            .filter(|total| *total <= MAX_TIER2_AGGREGATE_BYTES)
            .ok_or(Invalid(
                "legacy asset tree exceeds its aggregate byte bound",
            ))?;
        files.push(LegacyAssetFile {
            path,
            hash,
            sha1,
            size,
        });
    }
    files.sort_unstable_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
    Ok(Some(LegacyAssets { layout, files }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12";

    fn index(flags: &str, objects: serde_json::Value) -> Vec<u8> {
        let mut index = serde_json::json!({ "objects": objects });
        for flag in flags.split(',').filter(|flag| !flag.is_empty()) {
            index[flag] = true.into();
        }
        serde_json::to_vec(&index).unwrap()
    }

    fn error(bytes: &[u8]) -> String {
        legacy_assets(bytes).unwrap_err().to_string()
    }

    #[test]
    fn flags_select_the_tree_old_clients_read() {
        let objects = serde_json::json!({
            "sound/step.ogg": { "hash": HASH.to_uppercase(), "size": 3 },
            "icons/icon.png": { "hash": HASH, "size": 3 }
        });
        assert!(
            legacy_assets(&index("", objects.clone()))
                .unwrap()
                .is_none()
        );
        for (flags, layout) in [
            ("virtual", LegacyAssetLayout::Virtual),
            ("map_to_resources", LegacyAssetLayout::Resources),
            ("virtual,map_to_resources", LegacyAssetLayout::Resources),
        ] {
            let assets = legacy_assets(&index(flags, objects.clone()))
                .unwrap()
                .unwrap();
            assert_eq!(assets.layout, layout);
            let files = assets
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.object_path(), file.size))
                .collect::<Vec<_>>();
            let object = format!("objects/2f/{HASH}");
            assert_eq!(
                files,
                [
                    ("icons/icon.png", object.clone(), 3),
                    ("sound/step.ogg", object, 3)
                ]
            );
        }
    }

    #[test]
    fn modern_index_does_not_project_its_objects() {
        let modern = br#"{"objects":{"../outside":{"hash":"not-a-digest","size":-1}}}"#;
        assert!(legacy_assets(modern).unwrap().is_none());
        assert!(matches!(
            legacy_assets(br#"{"objects":{},"virtual":"yes"}"#),
            Err(AssetIndexError::Parse(_))
        ));
    }

    #[test]
    fn unsafe_or_aliasing_names_are_refused() {
        for (objects, expected) in [
            (
                serde_json::json!({ "../escape.ogg": { "hash": HASH, "size": 1 } }),
                "legacy asset path is unsafe",
            ),
            (
                serde_json::json!({
                    "sounds/STEP.ogg": { "hash": HASH, "size": 1 },
                    "sounds/step.ogg": { "hash": HASH, "size": 1 }
                }),
                "legacy asset paths contain a portable alias collision",
            ),
            (
                serde_json::json!({ "a": { "hash": "a", "size": 1 } }),
                "asset object digest is invalid",
            ),
            (
                serde_json::json!({
                    "a": { "hash": HASH, "size": 1 },
                    "b": { "hash": HASH, "size": 2 }
                }),
                "asset object digest has conflicting sizes",
            ),
        ] {
            assert_eq!(error(&index("virtual", objects)), expected);
        }
    }

    #[test]
    fn repeated_objects_count_toward_the_tree_byte_bound() {
        let copies = (MAX_TIER2_AGGREGATE_BYTES / MAX_TIER2_ARTIFACT_BYTES) + 1;
        let objects = (0..copies)
            .map(|copy| {
                (
                    format!("sounds/{copy}.ogg"),
                    serde_json::json!({ "hash": HASH, "size": MAX_TIER2_ARTIFACT_BYTES }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        assert_eq!(
            error(&index("map_to_resources", objects.into())),
            "legacy asset tree exceeds its aggregate byte bound"
        );
    }
}
