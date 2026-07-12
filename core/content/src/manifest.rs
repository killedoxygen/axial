use crate::error::{ContentError, ContentResult};
use crate::model::{
    CanonicalId, ContentDependency, ContentKind, FileRef, ProviderId, VersionIdentity,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "axial.content.json";
const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrySource {
    /// Installed through Discover; the launcher owns it end to end.
    Managed,
    /// A file that was already present and identified after the fact.
    Imported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub kind: ContentKind,
    /// Enabled-state base filename (no `.disabled` suffix), relative to the
    /// kind's install subdirectory.
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha512: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default)]
    pub dependencies: Vec<ContentDependency>,
    pub enabled: bool,
    pub source: EntrySource,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl ManifestEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn managed(
        canonical_id: CanonicalId,
        provider: ProviderId,
        project_id: String,
        version_id: String,
        kind: ContentKind,
        file: &FileRef,
        dependencies: Vec<ContentDependency>,
        title: Option<String>,
    ) -> Self {
        Self {
            canonical_id,
            provider,
            project_id,
            version_id,
            kind,
            filename: file.filename.clone(),
            sha1: file.sha1.clone(),
            sha512: file.sha512.clone(),
            size: file.size,
            dependencies,
            enabled: true,
            source: EntrySource::Managed,
            installed_at: now_rfc3339(),
            title,
        }
    }

    pub fn imported(kind: ContentKind, filename: String, identity: VersionIdentity) -> Self {
        Self {
            canonical_id: CanonicalId::for_project(identity.provider, &identity.project_id),
            provider: identity.provider,
            project_id: identity.project_id,
            version_id: identity.version_id,
            kind,
            filename,
            sha1: None,
            sha512: None,
            size: None,
            dependencies: Vec::new(),
            enabled: true,
            source: EntrySource::Imported,
            installed_at: now_rfc3339(),
            title: identity.title,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<ManifestEntry>,
}

impl Default for ContentManifest {
    fn default() -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl ContentManifest {
    pub fn load(game_dir: &Path) -> ContentResult<Self> {
        let path = manifest_path(game_dir);
        match fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ContentError::Io(error)),
        }
    }

    pub fn save(&self, game_dir: &Path) -> ContentResult<()> {
        let path = manifest_path(game_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self)?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, &body)?;
        fs::rename(&temp, &path)?;
        Ok(())
    }

    pub fn find(&self, canonical_id: &CanonicalId) -> Option<&ManifestEntry> {
        self.entries
            .iter()
            .find(|entry| &entry.canonical_id == canonical_id)
    }

    /// Insert or replace an entry by canonical id, returning the file that was
    /// previously recorded when it differs from the new one (so the caller can
    /// clean up the stale file on disk).
    pub fn upsert(&mut self, entry: ManifestEntry) -> Option<String> {
        let previous_filename = self
            .entries
            .iter()
            .position(|existing| existing.canonical_id == entry.canonical_id)
            .map(|index| self.entries.remove(index))
            .filter(|previous| previous.filename != entry.filename)
            .map(|previous| previous.filename);
        self.entries.push(entry);
        previous_filename
    }

    pub fn remove(&mut self, canonical_id: &CanonicalId) -> Option<ManifestEntry> {
        self.entries
            .iter()
            .position(|entry| &entry.canonical_id == canonical_id)
            .map(|index| self.entries.remove(index))
    }
}

/// The result of comparing the manifest against what is actually on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Managed/imported entries whose file is missing on disk.
    pub missing: Vec<CanonicalId>,
    /// Files on disk in a managed subdirectory that no entry accounts for.
    pub unmanaged: Vec<UnmanagedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmanagedFile {
    pub kind: ContentKind,
    pub filename: String,
    pub path: PathBuf,
}

/// Pure filesystem reconcile: flags entries whose file vanished and files that no
/// entry accounts for. Network identification of unmanaged files happens a layer
/// up (it needs a provider).
pub fn reconcile(game_dir: &Path, manifest: &ContentManifest) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    let recorded: HashSet<(ContentKind, String)> = manifest
        .entries
        .iter()
        .map(|entry| (entry.kind, entry.filename.clone()))
        .collect();

    for entry in &manifest.entries {
        if !entry_file_present(game_dir, entry) {
            report.missing.push(entry.canonical_id.clone());
        }
    }

    for kind in [
        ContentKind::Mod,
        ContentKind::ResourcePack,
        ContentKind::ShaderPack,
    ] {
        let Some(kind_dir) = kind.install_subdir() else {
            continue;
        };
        let dir = game_dir.join(kind_dir);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for dir_entry in entries.filter_map(Result::ok) {
            if !dir_entry.path().is_file() {
                continue;
            }
            let raw = dir_entry.file_name().to_string_lossy().to_string();
            let base = enabled_base_name(&raw);
            if recorded.contains(&(kind, base.clone())) {
                continue;
            }
            report.unmanaged.push(UnmanagedFile {
                kind,
                filename: base,
                path: dir_entry.path(),
            });
        }
    }

    report
}

fn entry_file_present(game_dir: &Path, entry: &ManifestEntry) -> bool {
    // A modpack entry records which pack the instance came from and owns no file
    // of its own, so it can never go missing.
    let Some(kind_dir) = entry.kind.install_subdir() else {
        return true;
    };
    let dir = game_dir.join(kind_dir);
    dir.join(&entry.filename).is_file()
        || dir.join(format!("{}.disabled", entry.filename)).is_file()
}

pub fn manifest_path(game_dir: &Path) -> PathBuf {
    game_dir.join(MANIFEST_FILE)
}

pub fn enabled_base_name(filename: &str) -> String {
    filename
        .strip_suffix(".disabled")
        .unwrap_or(filename)
        .to_string()
}

pub fn sha512_file(path: &Path) -> ContentResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn default_schema_version() -> u32 {
    MANIFEST_SCHEMA_VERSION
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileRef;

    fn temp_game_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-content-manifest-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create temp game dir");
        path
    }

    fn file_ref(filename: &str) -> FileRef {
        FileRef {
            url: format!("https://example.invalid/{filename}"),
            filename: filename.to_string(),
            sha1: Some("a".repeat(40)),
            sha512: Some("b".repeat(128)),
            size: Some(1024),
            primary: true,
        }
    }

    fn managed_entry(project: &str, filename: &str) -> ManifestEntry {
        ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, project),
            ProviderId::Modrinth,
            project.to_string(),
            format!("{project}-v1"),
            ContentKind::Mod,
            &file_ref(filename),
            Vec::new(),
            Some(project.to_string()),
        )
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = temp_game_dir("roundtrip");
        let mut manifest = ContentManifest::default();
        manifest.upsert(managed_entry("AAA", "sodium.jar"));
        manifest.save(&dir).expect("save");

        let loaded = ContentManifest::load(&dir).expect("load");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].filename, "sodium.jar");
        assert_eq!(loaded.schema_version, MANIFEST_SCHEMA_VERSION);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_manifest_is_empty() {
        let dir = temp_game_dir("missing");
        let loaded = ContentManifest::load(&dir).expect("load");
        assert!(loaded.entries.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_reports_stale_filename_only_when_changed() {
        let mut manifest = ContentManifest::default();
        assert_eq!(
            manifest.upsert(managed_entry("AAA", "sodium-0.5.jar")),
            None
        );
        assert_eq!(
            manifest.upsert(managed_entry("AAA", "sodium-0.6.jar")),
            Some("sodium-0.5.jar".to_string())
        );
        assert_eq!(
            manifest.upsert(managed_entry("AAA", "sodium-0.6.jar")),
            None
        );
        assert_eq!(manifest.entries.len(), 1);
    }

    #[test]
    fn reconcile_flags_missing_and_unmanaged() {
        let dir = temp_game_dir("reconcile");
        let mods_dir = dir.join("mods");
        fs::create_dir_all(&mods_dir).expect("mods dir");
        fs::write(mods_dir.join("present.jar"), b"jar").expect("present");
        fs::write(mods_dir.join("dropped.jar"), b"jar").expect("dropped");

        let mut manifest = ContentManifest::default();
        manifest.upsert(managed_entry("AAA", "present.jar"));
        manifest.upsert(managed_entry("BBB", "vanished.jar"));

        let report = reconcile(&dir, &manifest);
        assert_eq!(
            report.missing,
            vec![CanonicalId::for_project(ProviderId::Modrinth, "BBB")]
        );
        assert_eq!(report.unmanaged.len(), 1);
        assert_eq!(report.unmanaged[0].filename, "dropped.jar");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reconcile_treats_disabled_suffix_as_same_file() {
        let dir = temp_game_dir("disabled");
        let mods_dir = dir.join("mods");
        fs::create_dir_all(&mods_dir).expect("mods dir");
        fs::write(mods_dir.join("sodium.jar.disabled"), b"jar").expect("disabled file");

        let mut manifest = ContentManifest::default();
        manifest.upsert(managed_entry("AAA", "sodium.jar"));

        let report = reconcile(&dir, &manifest);
        assert!(report.missing.is_empty(), "disabled file still counts");
        assert!(report.unmanaged.is_empty(), "disabled file is recorded");
        fs::remove_dir_all(&dir).ok();
    }
}
