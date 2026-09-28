use axial_minecraft::portable_path::PortableFileName;
use axial_minecraft::{
    ManifestEntry, VersionEntry, VersionManifest, VersionSubjectKind, analyze_minecraft_version,
    manifest_release_references,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;
use url::Url;

pub const MAX_MANIFEST_BYTES: usize = 8 << 20;
pub const MAX_VERSION_BYTES: usize = 8 << 20;
const MAX_MANIFEST_ENTRIES: usize = 16_384;

/// Closed, safe failure labels. Provider text and local paths are never public copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFailure {
    Unavailable,
    Malformed,
    Cancelled,
    CacheUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStateId {
    Ready,
    Empty,
    Stale,
    Malformed,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogState {
    pub state_id: CatalogStateId,
    pub label: String,
    pub fresh: bool,
    pub stale: bool,
    pub cache_hit: bool,
    /// A stale empty catalog stays explicitly empty rather than appearing unavailable.
    pub empty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<CatalogFailure>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub versions: Vec<VersionEntry>,
    pub catalog_state: CatalogState,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CatalogError {
    #[error("Minecraft versions are temporarily unavailable. Try again.")]
    Unavailable,
    #[error("Minecraft version metadata is invalid. Try refreshing the catalog.")]
    Malformed,
    #[error("The selected Minecraft version is not in the current catalog.")]
    UnknownVersion,
    #[error("The version request was cancelled.")]
    Cancelled,
    #[error("Could not verify installed versions. Check the library folder and try again.")]
    InstalledUnavailable,
}

impl CatalogError {
    pub(crate) fn failure(&self) -> CatalogFailure {
        match self {
            Self::Malformed => CatalogFailure::Malformed,
            Self::Cancelled => CatalogFailure::Cancelled,
            _ => CatalogFailure::Unavailable,
        }
    }
}

/// An exact provider record admitted by the backend. Callers cannot fabricate URLs or hashes.
#[derive(Clone, Debug)]
pub struct VersionDescriptor {
    entry: ManifestEntry,
}

impl VersionDescriptor {
    pub fn id(&self) -> &str {
        &self.entry.id
    }

    pub fn metadata_url(&self) -> &str {
        &self.entry.url
    }

    pub fn metadata_sha1(&self) -> &str {
        &self.entry.sha1
    }

    pub(crate) fn new(entry: ManifestEntry) -> Self {
        Self { entry }
    }
}

pub(crate) fn decode_manifest(bytes: &[u8]) -> Result<VersionManifest, CatalogError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(CatalogError::Malformed);
    }
    let manifest: VersionManifest =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Malformed)?;
    if manifest.versions.len() > MAX_MANIFEST_ENTRIES {
        return Err(CatalogError::Malformed);
    }
    let mut ids = HashSet::new();
    let mut portable_ids = HashSet::new();
    for entry in &manifest.versions {
        let id = PortableFileName::new_exact(&entry.id).map_err(|_| CatalogError::Malformed)?;
        id.with_suffix(".json")
            .map_err(|_| CatalogError::Malformed)?;
        if !ids.insert(entry.id.as_str()) || !portable_ids.insert(id.key()) {
            return Err(CatalogError::Malformed);
        }
        if entry.kind.len() > 128
            || entry.kind.chars().any(char::is_control)
            || entry.release_time.len() > 128
            || entry.time.len() > 128
            || entry.sha1.len() != 40
            || !entry.sha1.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !metadata_url_allowed(&entry.url)
        {
            return Err(CatalogError::Malformed);
        }
    }
    for latest in [&manifest.latest.release, &manifest.latest.snapshot] {
        if !latest.is_empty() && !ids.contains(latest.as_str()) {
            return Err(CatalogError::Malformed);
        }
    }
    Ok(manifest)
}

pub(crate) fn metadata_url_allowed(raw: &str) -> bool {
    if raw.len() > 4096 {
        return false;
    }
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && url.fragment().is_none()
        && matches!(
            url.host_str(),
            Some("piston-meta.mojang.com" | "launchermeta.mojang.com" | "launcher.mojang.com")
        )
}

pub(crate) fn catalog_rows(manifest: &VersionManifest) -> Vec<VersionEntry> {
    let releases = manifest_release_references(manifest);
    // Catalog order is Mojang's order; installed rows use the domain comparator instead.
    manifest
        .versions
        .iter()
        .map(|entry| {
            let analysis = analyze_minecraft_version(
                &entry.id,
                &entry.kind,
                &entry.release_time,
                None,
                &releases,
            );
            VersionEntry {
                subject_kind: VersionSubjectKind::MinecraftVersion,
                id: entry.id.clone(),
                raw_kind: entry.kind.clone(),
                release_time: entry.release_time.clone(),
                minecraft_meta: analysis.minecraft_meta,
                lifecycle: analysis.lifecycle,
                inherits_from: String::new(),
                launchable: false,
                installed: false,
                status: "available".into(),
                status_detail: String::new(),
                needs_install: entry.id.clone(),
                java_component: String::new(),
                java_major: 0,
                loader: None,
            }
        })
        .collect()
}

pub(crate) fn snapshot_from(
    manifest: Option<&VersionManifest>,
    fresh: bool,
    cache_hit: bool,
    failure: Option<CatalogFailure>,
) -> CatalogSnapshot {
    let versions = manifest.map(catalog_rows).unwrap_or_default();
    let empty = manifest.is_some() && versions.is_empty();
    let stale = manifest.is_some() && !fresh;
    let (state_id, label) = if stale {
        (CatalogStateId::Stale, "Showing cached Minecraft versions")
    } else if manifest.is_some() && empty {
        (CatalogStateId::Empty, "No Minecraft versions available")
    } else if manifest.is_some() {
        (CatalogStateId::Ready, "Minecraft versions ready")
    } else if failure == Some(CatalogFailure::Malformed) {
        (
            CatalogStateId::Malformed,
            "Minecraft version catalog is invalid",
        )
    } else {
        (
            CatalogStateId::Unavailable,
            "Minecraft versions unavailable",
        )
    };
    CatalogSnapshot {
        versions,
        catalog_state: CatalogState {
            state_id,
            label: label.into(),
            fresh,
            stale,
            cache_hit,
            empty,
            failure,
        },
    }
}
