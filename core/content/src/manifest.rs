use crate::error::{ContentError, ContentResult};
use crate::model::{CanonicalId, ContentDependency, ContentKind, FileRef, ProviderId};
use crate::transaction::promote_replacement;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MANIFEST_FILE: &str = "axial.content.json";
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MANIFEST_TEMP_PREFIX: &str = ".axial.content.json.tmp";
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 4096;
const MAX_ENTRY_DEPENDENCIES: usize = 256;
const MAX_ID_BYTES: usize = 512;
const MAX_FILENAME_BYTES: usize = 255;
const MAX_TITLE_BYTES: usize = 1024;
const MAX_INSTALLED_AT_BYTES: usize = 64;
const MANIFEST_TEMP_ATTEMPTS: usize = 16;
static MANIFEST_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default, with = "strict_dependencies")]
    pub dependencies: Vec<ContentDependency>,
    pub enabled: bool,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

mod strict_dependencies {
    use crate::model::{ContentDependency, DependencyKind};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictDependency {
        #[serde(default)]
        project_id: Option<String>,
        #[serde(default)]
        version_id: Option<String>,
        kind: DependencyKind,
    }

    pub(super) fn serialize<S>(
        dependencies: &[ContentDependency],
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        dependencies.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<ContentDependency>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<StrictDependency>::deserialize(deserializer).map(|dependencies| {
            dependencies
                .into_iter()
                .map(|dependency| ContentDependency {
                    project_id: dependency.project_id,
                    version_id: dependency.version_id,
                    kind: dependency.kind,
                })
                .collect()
        })
    }
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
            installed_at: now_rfc3339(),
            title,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentManifest {
    pub schema_version: u32,
    pub entries: Vec<ManifestEntry>,
    #[doc(hidden)]
    #[serde(skip)]
    pub origin: ManifestOrigin,
}

#[derive(Debug, Clone, Copy, Default)]
#[doc(hidden)]
pub enum ManifestOrigin {
    #[default]
    Untracked,
    Missing,
    Present([u8; 32]),
}

impl PartialEq for ContentManifest {
    fn eq(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version && self.entries == other.entries
    }
}

impl Eq for ContentManifest {}

impl Default for ContentManifest {
    fn default() -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            entries: Vec::new(),
            origin: ManifestOrigin::Untracked,
        }
    }
}

impl ContentManifest {
    pub fn load(game_dir: &Path) -> ContentResult<Self> {
        let path = manifest_path(game_dir);
        let Some(bytes) = read_manifest_bytes(&path)? else {
            return Ok(Self {
                origin: ManifestOrigin::Missing,
                ..Self::default()
            });
        };
        Self::parse_and_validate(&bytes)
    }

    fn parse_and_validate(bytes: &[u8]) -> ContentResult<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ContentError::Invalid(
                "content manifest exceeds its size bound".to_string(),
            ));
        }
        let mut manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate()?;
        manifest.origin = ManifestOrigin::Present(manifest_digest(bytes));
        Ok(manifest)
    }

    /// Persist this launcher-owned manifest.
    ///
    /// Mutation callers must exclusively serialize the complete load, filesystem
    /// effect, and save transaction. Origin validation detects stale snapshots
    /// observed before promotion; it is not a cross-process compare-and-swap.
    pub fn save(&mut self, game_dir: &Path) -> ContentResult<()> {
        self.save_with_before_commit(game_dir, || {})
    }

    fn save_with_before_commit<F>(&mut self, game_dir: &Path, before_commit: F) -> ContentResult<()>
    where
        F: FnOnce(),
    {
        self.validate()?;
        let path = manifest_path(game_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let current = read_valid_manifest_snapshot(&path)?;
        self.validate_origin(current.as_deref())?;
        let body = serde_json::to_vec_pretty(self)?;
        if body.len() > MAX_MANIFEST_BYTES {
            return Err(ContentError::Invalid(
                "content manifest exceeds its size bound".to_string(),
            ));
        }
        let (temp, mut file) = create_manifest_temp(game_dir)?;
        let result = (|| {
            file.write_all(&body)?;
            file.sync_all()?;
            drop(file);

            before_commit();
            if read_valid_manifest_snapshot(&path)? != current {
                return Err(ContentError::Invalid(
                    "content manifest changed while it was being saved".to_string(),
                ));
            }
            promote_replacement(&temp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        } else {
            self.origin = ManifestOrigin::Present(manifest_digest(&body));
        }
        result
    }

    pub fn find(&self, canonical_id: &CanonicalId) -> Option<&ManifestEntry> {
        self.entries
            .iter()
            .find(|entry| &entry.canonical_id == canonical_id)
    }

    /// Validate provider-authored fields before they become launcher-owned
    /// provenance. Existing manifest conflicts remain subject to normal local
    /// ownership validation.
    pub fn validate_provider_entry(&self, entry: &ManifestEntry) -> ContentResult<()> {
        validate_entry(entry).map_err(|_| {
            ContentError::ProviderMetadataInvalid(
                "content metadata cannot be represented in the managed manifest".to_string(),
            )
        })?;
        if self.find(&entry.canonical_id).is_none() && self.entries.len() >= MAX_MANIFEST_ENTRIES {
            return Err(ContentError::ProviderMetadataInvalid(
                "content metadata exceeds the managed manifest entry bound".to_string(),
            ));
        }
        Ok(())
    }

    /// Validate the aggregate serialized bound after provider entries have been
    /// projected into a manifest, without reclassifying local ownership rules.
    pub fn validate_provider_projection(&self) -> ContentResult<()> {
        let body = serde_json::to_vec_pretty(self).map_err(|_| {
            ContentError::ProviderMetadataInvalid(
                "content metadata cannot be represented in the managed manifest".to_string(),
            )
        })?;
        if body.len() > MAX_MANIFEST_BYTES {
            return Err(ContentError::ProviderMetadataInvalid(
                "content metadata exceeds the managed manifest size bound".to_string(),
            ));
        }
        Ok(())
    }

    /// Insert or replace an entry by canonical id, returning the prior ownership
    /// record when its kind or filename changed so callers can clean the exact
    /// stale path.
    pub fn upsert(&mut self, entry: ManifestEntry) -> Option<ManifestEntry> {
        let displaced = self
            .entries
            .iter()
            .position(|existing| existing.canonical_id == entry.canonical_id)
            .map(|index| self.entries.remove(index))
            .filter(|previous| previous.kind != entry.kind || previous.filename != entry.filename);
        self.entries.push(entry);
        displaced
    }

    pub fn remove(&mut self, canonical_id: &CanonicalId) -> Option<ManifestEntry> {
        self.entries
            .iter()
            .position(|entry| &entry.canonical_id == canonical_id)
            .map(|index| self.entries.remove(index))
    }

    fn validate(&self) -> ContentResult<()> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ContentError::Invalid(format!(
                "unsupported content manifest schema version: {}",
                self.schema_version
            )));
        }
        if self.entries.len() > MAX_MANIFEST_ENTRIES {
            return Err(ContentError::Invalid(
                "content manifest has too many entries".to_string(),
            ));
        }

        let mut canonical_ids = HashSet::with_capacity(self.entries.len());
        let mut owned_files = HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            validate_entry(entry)?;
            if !canonical_ids.insert(entry.canonical_id.clone()) {
                return Err(ContentError::Invalid(
                    "content manifest contains a duplicate canonical id".to_string(),
                ));
            }
            let file_key = (entry.kind, entry.filename.to_ascii_lowercase());
            if !owned_files.insert(file_key) {
                return Err(ContentError::Invalid(
                    "content manifest contains a duplicate owned file".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn validate_origin(&self, current: Option<&[u8]>) -> ContentResult<()> {
        let unchanged = match (self.origin, current) {
            (ManifestOrigin::Untracked | ManifestOrigin::Missing, None) => true,
            (ManifestOrigin::Present(expected), Some(current)) => {
                manifest_digest(current) == expected
            }
            _ => false,
        };
        if unchanged {
            Ok(())
        } else {
            Err(ContentError::Invalid(
                "content manifest changed since it was loaded".to_string(),
            ))
        }
    }
}

fn manifest_digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn validate_entry(entry: &ManifestEntry) -> ContentResult<()> {
    validate_required_text("canonical id", entry.canonical_id.as_str(), MAX_ID_BYTES)?;
    validate_required_text("project id", &entry.project_id, MAX_ID_BYTES)?;
    validate_required_text("version id", &entry.version_id, MAX_ID_BYTES)?;
    if entry.canonical_id != CanonicalId::for_project(entry.provider, &entry.project_id) {
        return Err(ContentError::Invalid(
            "content manifest canonical id does not match its provider project".to_string(),
        ));
    }
    validate_filename(entry.kind, &entry.filename)?;
    validate_optional_hash("sha1", entry.sha1.as_deref(), 40)?;
    validate_optional_hash("sha512", entry.sha512.as_deref(), 128)?;
    if entry.kind != ContentKind::Modpack && entry.sha1.is_none() && entry.sha512.is_none() {
        return Err(ContentError::Invalid(
            "content manifest file entry is missing an integrity checksum".to_string(),
        ));
    }
    if entry.dependencies.len() > MAX_ENTRY_DEPENDENCIES {
        return Err(ContentError::Invalid(
            "content manifest entry has too many dependencies".to_string(),
        ));
    }
    for dependency in &entry.dependencies {
        if dependency.project_id.is_none() && dependency.version_id.is_none() {
            return Err(ContentError::Invalid(
                "content manifest dependency has no project or version identity".to_string(),
            ));
        }
        if let Some(project_id) = dependency.project_id.as_deref() {
            validate_required_text("dependency project id", project_id, MAX_ID_BYTES)?;
        }
        if let Some(version_id) = dependency.version_id.as_deref() {
            validate_required_text("dependency version id", version_id, MAX_ID_BYTES)?;
        }
    }
    validate_required_text(
        "installed timestamp",
        &entry.installed_at,
        MAX_INSTALLED_AT_BYTES,
    )?;
    if chrono::DateTime::parse_from_rfc3339(&entry.installed_at).is_err() {
        return Err(ContentError::Invalid(
            "content manifest has an invalid installed timestamp".to_string(),
        ));
    }
    if let Some(title) = entry.title.as_deref()
        && title.len() > MAX_TITLE_BYTES
    {
        return Err(ContentError::Invalid(
            "content manifest title exceeds its size bound".to_string(),
        ));
    }
    Ok(())
}

fn validate_required_text(label: &str, value: &str, max_bytes: usize) -> ContentResult<()> {
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(ContentError::Invalid(format!(
            "content manifest {label} is invalid"
        )));
    }
    Ok(())
}

fn validate_filename(kind: ContentKind, filename: &str) -> ContentResult<()> {
    if kind == ContentKind::Modpack {
        if filename.is_empty() {
            return Ok(());
        }
    } else {
        validate_required_text("filename", filename, MAX_FILENAME_BYTES)?;
    }
    if filename.len() > MAX_FILENAME_BYTES
        || filename == "."
        || filename == ".."
        || filename.contains(['/', '\\', '\0'])
        || filename.to_ascii_lowercase().ends_with(".disabled")
    {
        return Err(ContentError::Invalid(
            "content manifest filename is invalid".to_string(),
        ));
    }
    Ok(())
}

fn validate_optional_hash(label: &str, value: Option<&str>, length: usize) -> ContentResult<()> {
    if value.is_some_and(|value| {
        value.len() != length || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(ContentError::Invalid(format!(
            "content manifest {label} is invalid"
        )));
    }
    Ok(())
}

fn read_manifest_bytes(path: &Path) -> ContentResult<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ContentError::Io(error)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ContentError::Invalid(
            "content manifest path is not a regular file".to_string(),
        ));
    }
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(ContentError::Invalid(
            "content manifest exceeds its size bound".to_string(),
        ));
    }

    let file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ContentError::Invalid(
            "content manifest exceeds its size bound".to_string(),
        ));
    }
    Ok(Some(bytes))
}

fn read_valid_manifest_snapshot(path: &Path) -> ContentResult<Option<Vec<u8>>> {
    let Some(bytes) = read_manifest_bytes(path)? else {
        return Ok(None);
    };
    ContentManifest::parse_and_validate(&bytes)?;
    Ok(Some(bytes))
}

fn create_manifest_temp(game_dir: &Path) -> ContentResult<(PathBuf, fs::File)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..MANIFEST_TEMP_ATTEMPTS {
        let sequence = MANIFEST_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = game_dir.join(format!(
            "{MANIFEST_TEMP_PREFIX}-{}-{nanos:x}-{sequence:x}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ContentError::Io(error)),
        }
    }
    Err(ContentError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique content manifest temporary file",
    )))
}

/// Whether an entry still owns a regular file on disk. When provenance carries
/// integrity metadata, presence includes matching the recorded size and
/// strongest available hash so a same-name manual replacement is not trusted.
pub fn entry_file_present(game_dir: &Path, entry: &ManifestEntry) -> bool {
    matching_entry_filename(game_dir, entry).is_some()
}

fn matching_entry_filename(game_dir: &Path, entry: &ManifestEntry) -> Option<String> {
    // A modpack entry records which pack the instance came from and owns no file
    // of its own, so it can never go missing.
    let Some(kind_dir) = entry.kind.install_subdir() else {
        return Some(entry.filename.clone());
    };
    let dir = game_dir.join(kind_dir);
    let enabled = entry.filename.clone();
    let disabled = format!("{}.disabled", entry.filename);
    let variants = if entry.enabled {
        [enabled, disabled]
    } else {
        [disabled, enabled]
    };
    variants
        .into_iter()
        .find(|filename| entry_path_matches(&dir.join(filename), entry))
}

/// Whether one exact on-disk path still matches the integrity recorded for a
/// manifest entry. Callers that are about to replace a particular variant must
/// use this rather than accepting ownership from the filename alone.
pub fn entry_path_matches(path: &Path, entry: &ManifestEntry) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || entry.size.is_some_and(|size| size != metadata.len()) {
        return false;
    }
    if let Some(expected) = entry.sha512.as_deref() {
        return hash_file::<Sha512>(path).is_ok_and(|actual| actual.eq_ignore_ascii_case(expected));
    }
    if let Some(expected) = entry.sha1.as_deref() {
        return hash_file::<Sha1>(path).is_ok_and(|actual| actual.eq_ignore_ascii_case(expected));
    }
    false
}

pub fn manifest_path(game_dir: &Path) -> PathBuf {
    game_dir.join(MANIFEST_FILE)
}

pub fn sha512_file(path: &Path) -> ContentResult<String> {
    hash_file::<Sha512>(path)
}

fn hash_file<D>(path: &Path) -> ContentResult<String>
where
    D: Digest + Default,
{
    let mut file = fs::File::open(path)?;
    let mut hasher = D::default();
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
            sha1: Some("0".repeat(40)),
            sha512: None,
            size: None,
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
    fn save_replaces_an_existing_manifest() {
        let dir = temp_game_dir("replace");
        let mut manifest = ContentManifest::default();
        manifest.upsert(managed_entry("AAA", "old.jar"));
        manifest.save(&dir).expect("initial save");
        manifest.upsert(managed_entry("AAA", "new.jar"));

        manifest.save(&dir).expect("replacement save");

        let loaded = ContentManifest::load(&dir).expect("load replacement");
        assert_eq!(loaded.entries[0].filename, "new.jar");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_detects_a_manifest_change_before_final_validation() {
        let dir = temp_game_dir("changed-during-save");
        let mut initial = ContentManifest::default();
        initial.upsert(managed_entry("AAA", "initial.jar"));
        initial.save(&dir).expect("initial save");

        let mut replacement = initial.clone();
        replacement.upsert(managed_entry("AAA", "replacement.jar"));
        let mut external = ContentManifest::default();
        external.upsert(managed_entry("BBB", "external.jar"));
        let external_bytes = serde_json::to_vec_pretty(&external).expect("external manifest");
        let path = manifest_path(&dir);

        let error = replacement
            .save_with_before_commit(&dir, || {
                fs::write(&path, &external_bytes).expect("external replacement");
            })
            .expect_err("change observed before final validation must fail");

        assert!(matches!(error, ContentError::Invalid(_)));
        assert_eq!(
            ContentManifest::load(&dir).expect("load external manifest"),
            external
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_rejects_a_manifest_changed_since_load() {
        let dir = temp_game_dir("changed-since-load");
        let mut initial = ContentManifest::default();
        initial.upsert(managed_entry("AAA", "initial.jar"));
        initial.save(&dir).expect("initial save");
        let mut stale = ContentManifest::load(&dir).expect("load stale snapshot");

        let mut external = ContentManifest::load(&dir).expect("load external snapshot");
        external.upsert(managed_entry("BBB", "external.jar"));
        external.save(&dir).expect("external save");
        stale.upsert(managed_entry("CCC", "stale.jar"));

        let error = stale
            .save(&dir)
            .expect_err("stale snapshot must not overwrite external changes");

        assert!(matches!(error, ContentError::Invalid(_)));
        assert_eq!(
            ContentManifest::load(&dir).expect("load external manifest"),
            external
        );
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
    fn upsert_reports_displaced_ownership_only_when_changed() {
        let mut manifest = ContentManifest::default();
        let old = managed_entry("AAA", "sodium-0.5.jar");
        assert_eq!(manifest.upsert(old.clone()), None);
        assert_eq!(
            manifest.upsert(managed_entry("AAA", "sodium-0.6.jar")),
            Some(old)
        );
        assert_eq!(
            manifest.upsert(managed_entry("AAA", "sodium-0.6.jar")),
            None
        );
        assert_eq!(manifest.entries.len(), 1);
    }

    #[test]
    fn upsert_reports_displaced_ownership_when_kind_changes() {
        let mut manifest = ContentManifest::default();
        let previous = managed_entry("AAA", "shared.jar");
        manifest.upsert(previous.clone());
        let mut replacement = previous.clone();
        replacement.kind = ContentKind::ResourcePack;

        assert_eq!(manifest.upsert(replacement), Some(previous));
    }

    #[test]
    fn provider_entry_bounds_are_typed_without_reclassifying_local_manifest_errors() {
        let manifest = ContentManifest::default();
        let mut provider_entry = managed_entry("AAA", "managed.jar");
        provider_entry.title = Some("x".repeat(MAX_TITLE_BYTES + 1));

        assert!(matches!(
            manifest.validate_provider_entry(&provider_entry),
            Err(ContentError::ProviderMetadataInvalid(_))
        ));

        let mut local_manifest = ContentManifest::default();
        local_manifest.entries.push(provider_entry);
        assert!(matches!(
            local_manifest.validate(),
            Err(ContentError::Invalid(_))
        ));
    }

    #[test]
    fn load_requires_the_exact_v1_schema() {
        let dir = temp_game_dir("strict-schema");
        let path = manifest_path(&dir);
        let cases = [
            r#"{"entries":[]}"#,
            r#"{"schema_version":2,"entries":[]}"#,
            r#"{"schema_version":1,"entries":[],"unidentified":[]}"#,
            r#"{"schema_version":1,"entries":[],"extra":true}"#,
        ];

        for body in cases {
            fs::write(&path, body).expect("write malformed manifest");
            assert!(
                ContentManifest::load(&dir).is_err(),
                "manifest should fail closed: {body}"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_a_dependency_without_project_or_version_identity() {
        let dir = temp_game_dir("dependency-without-identity");
        let path = manifest_path(&dir);
        let mut value =
            serde_json::to_value(ContentManifest::default()).expect("default manifest JSON");
        let mut entry =
            serde_json::to_value(managed_entry("AAA", "managed.jar")).expect("managed entry JSON");
        entry["dependencies"] = serde_json::json!([{ "kind": "required" }]);
        value["entries"] = serde_json::json!([entry]);
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("invalid dependency manifest"),
        )
        .expect("write manifest");

        let error =
            ContentManifest::load(&dir).expect_err("identity-free dependency must fail closed");

        assert!(matches!(error, ContentError::Invalid(_)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_legacy_source_and_unknown_entry_fields() {
        let dir = temp_game_dir("strict-entry");
        let path = manifest_path(&dir);
        let entry = managed_entry("AAA", "sodium.jar");
        let mut value = serde_json::to_value(ContentManifest {
            entries: vec![entry],
            ..ContentManifest::default()
        })
        .expect("manifest value");

        value["entries"][0]["source"] = serde_json::json!("managed");
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("legacy source body"),
        )
        .expect("write legacy source");
        assert!(ContentManifest::load(&dir).is_err());

        value["entries"][0]
            .as_object_mut()
            .expect("entry object")
            .remove("source");
        value["entries"][0]["unknown"] = serde_json::json!(true);
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("unknown field body"),
        )
        .expect("write unknown field");
        assert!(ContentManifest::load(&dir).is_err());

        value["entries"][0]
            .as_object_mut()
            .expect("entry object")
            .remove("unknown");
        value["entries"][0]["dependencies"] = serde_json::json!([{
            "project_id": "dependency",
            "kind": "required",
            "unknown": true
        }]);
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("unknown dependency field body"),
        )
        .expect("write unknown dependency field");
        assert!(ContentManifest::load(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_duplicate_canonical_ids_and_owned_files() {
        let dir = temp_game_dir("duplicates");
        let path = manifest_path(&dir);
        let first = managed_entry("AAA", "first.jar");
        let mut duplicate_id = managed_entry("AAA", "second.jar");
        duplicate_id.version_id = "AAA-v2".to_string();
        let duplicate_ids = ContentManifest {
            entries: vec![first.clone(), duplicate_id],
            ..ContentManifest::default()
        };
        fs::write(
            &path,
            serde_json::to_vec(&duplicate_ids).expect("duplicate id body"),
        )
        .expect("write duplicate ids");
        assert!(ContentManifest::load(&dir).is_err());

        let duplicate_files = ContentManifest {
            entries: vec![first, managed_entry("BBB", "FIRST.JAR")],
            ..ContentManifest::default()
        };
        fs::write(
            &path,
            serde_json::to_vec(&duplicate_files).expect("duplicate file body"),
        )
        .expect("write duplicate files");
        assert!(ContentManifest::load(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_unbounded_or_unsafe_entries() {
        let dir = temp_game_dir("bounded-entry");
        let path = manifest_path(&dir);
        let mut entry = managed_entry("AAA", "tracked.jar");
        entry.filename = "x".repeat(MAX_FILENAME_BYTES + 1);
        fs::write(
            &path,
            serde_json::to_vec(&ContentManifest {
                entries: vec![entry],
                ..ContentManifest::default()
            })
            .expect("oversized entry body"),
        )
        .expect("write oversized entry");
        assert!(ContentManifest::load(&dir).is_err());

        let mut entry = managed_entry("AAA", "../tracked.jar");
        entry.sha512 = Some("not-a-hash".to_string());
        fs::write(
            &path,
            serde_json::to_vec(&ContentManifest {
                entries: vec![entry],
                ..ContentManifest::default()
            })
            .expect("unsafe entry body"),
        )
        .expect("write unsafe entry");
        assert!(ContentManifest::load(&dir).is_err());

        let entry = managed_entry("AAA", "tracked.jar.DISABLED");
        fs::write(
            &path,
            serde_json::to_vec(&ContentManifest {
                entries: vec![entry],
                ..ContentManifest::default()
            })
            .expect("disabled base filename body"),
        )
        .expect("write disabled base filename");
        assert!(ContentManifest::load(&dir).is_err());

        fs::write(&path, vec![b' '; MAX_MANIFEST_BYTES + 1]).expect("oversized manifest");
        assert!(ContentManifest::load(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_entries_require_integrity_before_they_can_own_a_path() {
        let dir = temp_game_dir("checksum-required");
        let path = manifest_path(&dir);
        let mut entry = managed_entry("AAA", "tracked.jar");
        entry.sha1 = None;
        entry.sha512 = None;
        fs::write(
            &path,
            serde_json::to_vec(&ContentManifest {
                entries: vec![entry.clone()],
                ..ContentManifest::default()
            })
            .expect("manifest body"),
        )
        .expect("write manifest");

        assert!(ContentManifest::load(&dir).is_err());
        let tracked = dir.join("mods").join("tracked.jar");
        fs::create_dir_all(tracked.parent().expect("mods dir")).expect("create mods");
        fs::write(&tracked, b"unverified").expect("write unverified file");
        assert!(!entry_path_matches(&tracked, &entry));

        fs::remove_file(&path).expect("remove rejected manifest");
        entry.kind = ContentKind::Modpack;
        entry.filename.clear();
        let mut manifest = ContentManifest::default();
        manifest.upsert(entry);
        manifest
            .save(&dir)
            .expect("save provenance-only pack entry");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn entry_presence_accepts_disabled_state_but_rejects_replacement() {
        let dir = temp_game_dir("entry-presence");
        let mods_dir = dir.join("mods");
        fs::create_dir_all(&mods_dir).expect("mods dir");
        let path = mods_dir.join("tracked.jar");
        fs::write(&path, b"old").expect("tracked file");

        let mut entry = managed_entry("AAA", "tracked.jar");
        entry.size = Some(3);
        entry.sha512 = Some(sha512_file(&path).expect("tracked hash"));
        assert!(entry_file_present(&dir, &entry));

        fs::write(&path, b"new").expect("replace tracked file");
        assert!(!entry_file_present(&dir, &entry));

        fs::write(&path, b"old").expect("restore tracked file");
        fs::rename(&path, mods_dir.join("tracked.jar.disabled")).expect("disable");
        assert!(entry_file_present(&dir, &entry));
        fs::remove_dir_all(&dir).ok();
    }
}
