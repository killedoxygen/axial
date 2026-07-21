use crate::error::{ContentError, ContentResult};
use crate::model::{
    CanonicalId, ContentDependency, ContentKind, FileRef, ManagedContentFileName, ProviderId,
};
use crate::transaction::promote_replacement;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MANIFEST_FILE: &str = "axial.content.json";
const MANIFEST_SCHEMA_VERSION: u32 = 3;
const MANIFEST_TEMP_PREFIX: &str = ".axial.content.json.tmp";
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 4096;
const MAX_ENTRY_DEPENDENCIES: usize = 256;
const MAX_ID_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 1024;
const MAX_INSTALLED_AT_BYTES: usize = 64;
const MANIFEST_TEMP_ATTEMPTS: usize = 16;
static MANIFEST_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestEntry {
    canonical_id: CanonicalId,
    provider: ProviderId,
    project_id: String,
    version_id: String,
    kind: ContentKind,
    /// Enabled-state base filename (no `.disabled` suffix), relative to the
    /// kind's install subdirectory. Provenance-only modpack entries omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<ManagedContentFileName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha512: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, serialize_with = "strict_dependencies::serialize")]
    dependencies: Vec<ContentDependency>,
    enabled: bool,
    installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

#[derive(Clone)]
pub struct PendingManifestEntry {
    canonical_id: CanonicalId,
    provider: ProviderId,
    project_id: String,
    version_id: String,
    kind: ContentKind,
    filename: ManagedContentFileName,
    sha512: String,
    dependencies: Vec<ContentDependency>,
    enabled: bool,
    installed_at: String,
    title: Option<String>,
}

#[derive(Serialize)]
struct PendingManifestEntryProjection<'a> {
    canonical_id: &'a CanonicalId,
    provider: ProviderId,
    project_id: &'a str,
    version_id: &'a str,
    kind: ContentKind,
    filename: &'a ManagedContentFileName,
    sha512: &'a str,
    // A quoted 20-digit value is two bytes longer than the longest serialized
    // u64 and therefore bounds the eventual numeric field without fabricating
    // a valid manifest entry.
    size: &'static str,
    #[serde(serialize_with = "strict_dependencies::serialize")]
    dependencies: &'a [ContentDependency],
    enabled: bool,
    installed_at: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ManifestProjectionEntry<'a> {
    Existing(&'a ManifestEntry),
    Pending(PendingManifestEntryProjection<'a>),
}

#[derive(Serialize)]
struct ContentManifestProjection<'a> {
    schema_version: u32,
    entries: Vec<ManifestProjectionEntry<'a>>,
}

#[derive(Serialize)]
struct ContentManifestRefProjection<'a> {
    schema_version: u32,
    entries: Vec<&'a ManifestEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntryWire {
    canonical_id: CanonicalId,
    provider: ProviderId,
    project_id: String,
    version_id: String,
    kind: ContentKind,
    #[serde(default)]
    filename: Option<ManagedContentFileName>,
    #[serde(default)]
    sha512: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default, deserialize_with = "strict_dependencies::deserialize")]
    dependencies: Vec<ContentDependency>,
    enabled: bool,
    installed_at: String,
    #[serde(default)]
    title: Option<String>,
}

impl From<ManifestEntryWire> for ManifestEntry {
    fn from(entry: ManifestEntryWire) -> Self {
        Self {
            canonical_id: entry.canonical_id,
            provider: entry.provider,
            project_id: entry.project_id,
            version_id: entry.version_id,
            kind: entry.kind,
            filename: entry.filename,
            sha512: entry.sha512,
            size: entry.size,
            dependencies: entry.dependencies,
            enabled: entry.enabled,
            installed_at: entry.installed_at,
            title: entry.title,
        }
    }
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
    ) -> ContentResult<Self> {
        let filename = ManagedContentFileName::new_exact(&file.filename).map_err(|_| {
            ContentError::ProviderMetadataInvalid(
                "the provider returned an invalid content filename".to_string(),
            )
        })?;
        Self::managed_file(
            canonical_id,
            provider,
            project_id,
            version_id,
            kind,
            filename,
            file.sha512.clone(),
            file.size,
            dependencies,
            title,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn managed_file(
        canonical_id: CanonicalId,
        provider: ProviderId,
        project_id: String,
        version_id: String,
        kind: ContentKind,
        filename: ManagedContentFileName,
        sha512: Option<String>,
        size: Option<u64>,
        dependencies: Vec<ContentDependency>,
        title: Option<String>,
    ) -> ContentResult<Self> {
        if kind == ContentKind::Modpack
            || (kind == ContentKind::Mod && !filename.key().as_str().ends_with(".jar"))
        {
            return Err(ContentError::ProviderMetadataInvalid(
                "the provider returned an invalid content filename".to_string(),
            ));
        }
        let entry = Self {
            canonical_id,
            provider,
            project_id,
            version_id,
            kind,
            filename: Some(filename),
            sha512,
            size,
            dependencies,
            enabled: true,
            installed_at: now_rfc3339(),
            title,
        };
        validate_entry(&entry).map_err(provider_manifest_error)?;
        Ok(entry)
    }

    pub fn provenance(
        canonical_id: CanonicalId,
        provider: ProviderId,
        project_id: String,
        version_id: String,
        title: Option<String>,
    ) -> ContentResult<Self> {
        let entry = Self {
            canonical_id,
            provider,
            project_id,
            version_id,
            kind: ContentKind::Modpack,
            filename: None,
            sha512: None,
            size: None,
            dependencies: Vec::new(),
            enabled: true,
            installed_at: now_rfc3339(),
            title,
        };
        validate_entry(&entry).map_err(provider_manifest_error)?;
        Ok(entry)
    }

    pub fn provider(&self) -> ProviderId {
        self.provider
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn version_id(&self) -> &str {
        &self.version_id
    }

    pub fn kind(&self) -> ContentKind {
        self.kind
    }

    pub fn managed_filename(&self) -> Option<&ManagedContentFileName> {
        self.filename.as_ref()
    }

    pub fn sha512(&self) -> Option<&str> {
        self.sha512.as_deref()
    }

    pub fn size(&self) -> Option<u64> {
        self.size
    }

    pub fn dependencies(&self) -> &[ContentDependency] {
        &self.dependencies
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn installed_at(&self) -> &str {
        &self.installed_at
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) -> bool {
        let changed = self.enabled != enabled;
        self.enabled = enabled;
        changed
    }

    pub(crate) fn record_authenticated_file(
        &mut self,
        size: u64,
        sha512: String,
    ) -> ContentResult<()> {
        let mut candidate = self.clone();
        candidate.size = Some(size);
        candidate.sha512 = Some(sha512);
        validate_entry(&candidate)?;
        self.size = candidate.size;
        self.sha512 = candidate.sha512;
        Ok(())
    }
}

impl PendingManifestEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn managed_file(
        canonical_id: CanonicalId,
        provider: ProviderId,
        project_id: String,
        version_id: String,
        kind: ContentKind,
        filename: ManagedContentFileName,
        sha512: String,
        dependencies: Vec<ContentDependency>,
        title: Option<String>,
    ) -> ContentResult<Self> {
        let entry = Self {
            canonical_id,
            provider,
            project_id,
            version_id,
            kind,
            filename,
            sha512,
            dependencies,
            enabled: true,
            installed_at: now_rfc3339(),
            title,
        };
        validate_pending_entry(&entry).map_err(provider_manifest_error)?;
        Ok(entry)
    }

    pub fn canonical_id(&self) -> &CanonicalId {
        &self.canonical_id
    }

    pub fn kind(&self) -> ContentKind {
        self.kind
    }

    pub fn filename(&self) -> &ManagedContentFileName {
        &self.filename
    }

    pub fn sha512(&self) -> &str {
        &self.sha512
    }

    pub fn materialize(self, size: u64) -> ContentResult<ManifestEntry> {
        if size == 0 {
            return Err(ContentError::ProviderMetadataInvalid(
                "identified modpack content has no positive authenticated size".to_string(),
            ));
        }
        let entry = ManifestEntry {
            canonical_id: self.canonical_id,
            provider: self.provider,
            project_id: self.project_id,
            version_id: self.version_id,
            kind: self.kind,
            filename: Some(self.filename),
            sha512: Some(self.sha512),
            size: Some(size),
            dependencies: self.dependencies,
            enabled: self.enabled,
            installed_at: self.installed_at,
            title: self.title,
        };
        validate_entry(&entry).map_err(provider_manifest_error)?;
        Ok(entry)
    }

    fn projection(&self) -> PendingManifestEntryProjection<'_> {
        PendingManifestEntryProjection {
            canonical_id: &self.canonical_id,
            provider: self.provider,
            project_id: &self.project_id,
            version_id: &self.version_id,
            kind: self.kind,
            filename: &self.filename,
            sha512: &self.sha512,
            size: "00000000000000000000",
            dependencies: &self.dependencies,
            enabled: self.enabled,
            installed_at: &self.installed_at,
            title: self.title.as_deref(),
        }
    }
}

fn provider_manifest_error(_: ContentError) -> ContentError {
    ContentError::ProviderMetadataInvalid(
        "content metadata cannot be represented in the managed manifest".to_string(),
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentManifest {
    schema_version: u32,
    entries: Vec<ManifestEntry>,
    #[serde(skip)]
    origin: ManifestOrigin,
}

#[derive(Debug, Clone, Copy, Default)]
enum ManifestOrigin {
    #[default]
    Untracked,
    Missing,
    Present([u8; 32]),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentManifestWire {
    schema_version: u32,
    entries: Vec<ManifestEntryWire>,
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
        let wire: ContentManifestWire = serde_json::from_slice(bytes)?;
        let mut manifest = Self {
            schema_version: wire.schema_version,
            entries: wire.entries.into_iter().map(ManifestEntry::from).collect(),
            origin: ManifestOrigin::Untracked,
        };
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
        self.save_with_revalidation(game_dir, || Ok(()))
    }

    pub(crate) fn save_with_revalidation<F>(
        &mut self,
        game_dir: &Path,
        revalidate: F,
    ) -> ContentResult<()>
    where
        F: FnOnce() -> ContentResult<()>,
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

            revalidate()?;
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

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
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

    pub fn validate_provider_pending_projection(
        &self,
        pending: &[PendingManifestEntry],
    ) -> ContentResult<()> {
        if pending.len() > MAX_MANIFEST_ENTRIES {
            return Err(ContentError::ProviderMetadataInvalid(
                "content metadata exceeds the managed manifest entry bound".to_string(),
            ));
        }
        let mut pending_ids = HashSet::with_capacity(pending.len());
        for entry in pending {
            validate_pending_entry(entry).map_err(provider_manifest_error)?;
            if !pending_ids.insert(entry.canonical_id.clone()) {
                return Err(ContentError::ProviderMetadataInvalid(
                    "content metadata repeats a canonical id".to_string(),
                ));
            }
        }

        let retained = self
            .entries
            .iter()
            .filter(|entry| !pending_ids.contains(entry.canonical_id()))
            .collect::<Vec<_>>();
        if retained
            .len()
            .checked_add(pending.len())
            .is_none_or(|total| total > MAX_MANIFEST_ENTRIES)
        {
            return Err(ContentError::ProviderMetadataInvalid(
                "content metadata exceeds the managed manifest entry bound".to_string(),
            ));
        }

        let mut owned_files = HashSet::with_capacity(retained.len() + pending.len());
        for entry in &retained {
            if let Some(filename) = entry.managed_filename() {
                owned_files.insert((entry.kind(), filename.key()));
            }
        }
        for entry in pending {
            if !owned_files.insert((entry.kind, entry.filename.key())) {
                return Err(ContentError::ProviderMetadataInvalid(
                    "content metadata repeats managed file ownership".to_string(),
                ));
            }
        }

        let entries = retained
            .into_iter()
            .map(ManifestProjectionEntry::Existing)
            .chain(pending.iter().map(|entry| {
                ManifestProjectionEntry::Pending(entry.projection())
            }))
            .collect();
        let body = serde_json::to_vec_pretty(&ContentManifestProjection {
            schema_version: MANIFEST_SCHEMA_VERSION,
            entries,
        })
        .map_err(|_| {
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
    pub fn try_upsert(&mut self, entry: ManifestEntry) -> ContentResult<Option<ManifestEntry>> {
        validate_entry(&entry)?;
        let existing_index = self
            .entries
            .iter()
            .position(|existing| existing.canonical_id == entry.canonical_id);
        if existing_index.is_none() && self.entries.len() >= MAX_MANIFEST_ENTRIES {
            return Err(ContentError::Invalid(
                "content manifest has too many entries".to_string(),
            ));
        }

        let incoming_file = entry
            .managed_filename()
            .map(|filename| (entry.kind(), filename.key()));
        if incoming_file.as_ref().is_some_and(|incoming| {
            self.entries.iter().enumerate().any(|(index, existing)| {
                Some(index) != existing_index
                    && existing
                        .managed_filename()
                        .is_some_and(|filename| &(existing.kind(), filename.key()) == incoming)
            })
        }) {
            return Err(ContentError::Invalid(
                "content manifest contains a duplicate owned file".to_string(),
            ));
        }
        self.validate_upsert_projection(&entry, existing_index)?;
        let previous = existing_index.map(|index| self.entries[index].clone());
        let displaced = previous
            .as_ref()
            .filter(|previous| previous.kind != entry.kind || previous.filename != entry.filename)
            .cloned();
        if let Some(index) = existing_index {
            self.entries[index] = entry;
        } else {
            self.entries.push(entry);
        }
        Ok(displaced)
    }

    pub fn try_upsert_batch(
        &mut self,
        additions: Vec<ManifestEntry>,
    ) -> ContentResult<Vec<ManifestEntry>> {
        if additions.len() > MAX_MANIFEST_ENTRIES {
            return Err(ContentError::Invalid(
                "content manifest has too many entries".to_string(),
            ));
        }
        let mut addition_ids = HashSet::with_capacity(additions.len());
        for entry in &additions {
            validate_entry(entry)?;
            if !addition_ids.insert(entry.canonical_id.clone()) {
                return Err(ContentError::Invalid(
                    "content manifest contains a duplicate canonical id".to_string(),
                ));
            }
        }

        let mut entries = self.entries.clone();
        let mut indexes = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.canonical_id.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut displaced = Vec::new();
        for entry in additions {
            if let Some(index) = indexes.get(&entry.canonical_id).copied() {
                let previous = std::mem::replace(&mut entries[index], entry);
                if previous.kind != entries[index].kind
                    || previous.filename != entries[index].filename
                {
                    displaced.push(previous);
                }
            } else {
                let canonical_id = entry.canonical_id.clone();
                indexes.insert(canonical_id, entries.len());
                entries.push(entry);
            }
        }
        let candidate = Self {
            schema_version: self.schema_version,
            entries,
            origin: self.origin,
        };
        candidate.validate()?;
        candidate.validate_serialized_bound()?;
        self.entries = candidate.entries;
        Ok(displaced)
    }

    pub fn remove(&mut self, canonical_id: &CanonicalId) -> Option<ManifestEntry> {
        self.entries
            .iter()
            .position(|entry| &entry.canonical_id == canonical_id)
            .map(|index| self.entries.remove(index))
    }

    pub fn try_set_enabled(
        &mut self,
        canonical_id: &CanonicalId,
        enabled: bool,
    ) -> ContentResult<Option<bool>> {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| &entry.canonical_id == canonical_id)
        else {
            return Ok(None);
        };
        if self.entries[index].enabled == enabled {
            return Ok(Some(false));
        }

        let mut candidate = self.entries[index].clone();
        candidate.enabled = enabled;
        self.validate_upsert_projection(&candidate, Some(index))?;
        self.entries[index] = candidate;
        Ok(Some(true))
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
            let Some(filename) = entry.managed_filename() else {
                continue;
            };
            let file_key = (entry.kind, filename.key());
            if !owned_files.insert(file_key) {
                return Err(ContentError::Invalid(
                    "content manifest contains a duplicate owned file".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn validate_upsert_projection(
        &self,
        entry: &ManifestEntry,
        existing_index: Option<usize>,
    ) -> ContentResult<()> {
        let entries = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, existing)| {
                if Some(index) == existing_index {
                    entry
                } else {
                    existing
                }
            })
            .chain(existing_index.is_none().then_some(entry))
            .collect::<Vec<_>>();
        let body = serde_json::to_vec_pretty(&ContentManifestRefProjection {
            schema_version: self.schema_version,
            entries,
        })?;
        if body.len() > MAX_MANIFEST_BYTES {
            return Err(ContentError::Invalid(
                "content manifest exceeds its size bound".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_serialized_bound(&self) -> ContentResult<()> {
        if serde_json::to_vec_pretty(self)?.len() > MAX_MANIFEST_BYTES {
            return Err(ContentError::Invalid(
                "content manifest exceeds its size bound".to_string(),
            ));
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
    validate_entry_identity(
        &entry.canonical_id,
        entry.provider,
        &entry.project_id,
        &entry.version_id,
    )?;
    validate_filename(entry.kind, entry.managed_filename())?;
    if entry.kind == ContentKind::Modpack {
        if entry.filename.is_some() || entry.sha512.is_some() || entry.size.is_some() {
            return Err(ContentError::Invalid(
                "content manifest modpack provenance cannot own a file".to_string(),
            ));
        }
    } else {
        validate_sha512(entry.sha512.as_deref())?;
        if !entry.size.is_some_and(|size| size > 0) {
            return Err(ContentError::Invalid(
                "content manifest file entry is missing an exact positive size".to_string(),
            ));
        }
    }
    validate_entry_metadata(&entry.dependencies, &entry.installed_at, entry.title.as_deref())
}

fn validate_pending_entry(entry: &PendingManifestEntry) -> ContentResult<()> {
    validate_entry_identity(
        &entry.canonical_id,
        entry.provider,
        &entry.project_id,
        &entry.version_id,
    )?;
    if entry.kind == ContentKind::Modpack
        || (entry.kind == ContentKind::Mod && !entry.filename.key().as_str().ends_with(".jar"))
    {
        return Err(ContentError::Invalid(
            "content manifest filename is invalid".to_string(),
        ));
    }
    validate_sha512(Some(&entry.sha512))?;
    validate_entry_metadata(&entry.dependencies, &entry.installed_at, entry.title.as_deref())
}

fn validate_entry_identity(
    canonical_id: &CanonicalId,
    provider: ProviderId,
    project_id: &str,
    version_id: &str,
) -> ContentResult<()> {
    validate_required_text("canonical id", canonical_id.as_str(), MAX_ID_BYTES)?;
    validate_required_text("project id", project_id, MAX_ID_BYTES)?;
    validate_required_text("version id", version_id, MAX_ID_BYTES)?;
    if canonical_id != &CanonicalId::for_project(provider, project_id) {
        return Err(ContentError::Invalid(
            "content manifest canonical id does not match its provider project".to_string(),
        ));
    }
    Ok(())
}

fn validate_entry_metadata(
    dependencies: &[ContentDependency],
    installed_at: &str,
    title: Option<&str>,
) -> ContentResult<()> {
    if dependencies.len() > MAX_ENTRY_DEPENDENCIES {
        return Err(ContentError::Invalid(
            "content manifest entry has too many dependencies".to_string(),
        ));
    }
    for dependency in dependencies {
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
        installed_at,
        MAX_INSTALLED_AT_BYTES,
    )?;
    if chrono::DateTime::parse_from_rfc3339(installed_at).is_err() {
        return Err(ContentError::Invalid(
            "content manifest has an invalid installed timestamp".to_string(),
        ));
    }
    if let Some(title) = title
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

fn validate_filename(
    kind: ContentKind,
    filename: Option<&ManagedContentFileName>,
) -> ContentResult<()> {
    if kind == ContentKind::Modpack {
        if filename.is_none() {
            return Ok(());
        }
    } else if filename.is_some() {
        return Ok(());
    }
    Err(ContentError::Invalid(
        "content manifest filename is invalid".to_string(),
    ))
}

fn validate_sha512(value: Option<&str>) -> ContentResult<()> {
    if !value.is_some_and(|value| {
        value.len() == 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(ContentError::Invalid(
            "content manifest sha512 is invalid".to_string(),
        ));
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

/// Whether an entry still owns a regular file on disk. Presence requires the
/// exact recorded size and SHA-512 so a same-name replacement is not trusted.
pub fn entry_file_present(game_dir: &Path, entry: &ManifestEntry) -> bool {
    matching_entry_filename(game_dir, entry).is_some()
}

fn matching_entry_filename(game_dir: &Path, entry: &ManifestEntry) -> Option<String> {
    // A modpack entry records which pack the instance came from and owns no file
    // of its own, so it can never go missing.
    let Some(kind_dir) = entry.kind.install_subdir() else {
        return (entry.filename.is_none() && entry.sha512.is_none() && entry.size.is_none())
            .then(String::new);
    };
    let dir = game_dir.join(kind_dir);
    let filename = entry
        .managed_filename()
        .expect("validated file-owning entries have a managed filename");
    let enabled = filename.to_string();
    let disabled = filename.disabled().to_string();
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
    let (Some(expected_size), Some(expected_sha512)) = (entry.size, entry.sha512.as_deref()) else {
        return false;
    };
    if expected_size == 0 || validate_sha512(Some(expected_sha512)).is_err() {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || expected_size != metadata.len() {
        return false;
    }
    hash_file::<Sha512>(path).is_ok_and(|actual| actual == expected_sha512)
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
            sha1: None,
            sha512: Some("0".repeat(128)),
            size: Some(1),
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
        .expect("valid managed manifest entry")
    }

    fn insert(
        manifest: &mut ContentManifest,
        entry: ManifestEntry,
    ) -> Option<ManifestEntry> {
        manifest.try_upsert(entry).expect("insert manifest entry")
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = temp_game_dir("roundtrip");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, managed_entry("AAA", "sodium.jar"));
        manifest.save(&dir).expect("save");

        let loaded = ContentManifest::load(&dir).expect("load");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(
            loaded.entries()[0]
                .managed_filename()
                .expect("managed filename")
                .as_str(),
            "sodium.jar"
        );
        assert_eq!(loaded.schema_version(), MANIFEST_SCHEMA_VERSION);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_replaces_an_existing_manifest() {
        let dir = temp_game_dir("replace");
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, managed_entry("AAA", "old.jar"));
        manifest.save(&dir).expect("initial save");
        insert(&mut manifest, managed_entry("AAA", "new.jar"));

        manifest.save(&dir).expect("replacement save");

        let loaded = ContentManifest::load(&dir).expect("load replacement");
        assert_eq!(
            loaded.entries()[0]
                .managed_filename()
                .expect("managed filename")
                .as_str(),
            "new.jar"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_detects_a_manifest_change_before_final_validation() {
        let dir = temp_game_dir("changed-during-save");
        let mut initial = ContentManifest::default();
        insert(&mut initial, managed_entry("AAA", "initial.jar"));
        initial.save(&dir).expect("initial save");

        let mut replacement = initial.clone();
        insert(&mut replacement, managed_entry("AAA", "replacement.jar"));
        let mut external = ContentManifest::default();
        insert(&mut external, managed_entry("BBB", "external.jar"));
        let external_bytes = serde_json::to_vec_pretty(&external).expect("external manifest");
        let path = manifest_path(&dir);

        let error = replacement
            .save_with_revalidation(&dir, || {
                fs::write(&path, &external_bytes).expect("external replacement");
                Ok(())
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
        insert(&mut initial, managed_entry("AAA", "initial.jar"));
        initial.save(&dir).expect("initial save");
        let mut stale = ContentManifest::load(&dir).expect("load stale snapshot");

        let mut external = ContentManifest::load(&dir).expect("load external snapshot");
        insert(&mut external, managed_entry("BBB", "external.jar"));
        external.save(&dir).expect("external save");
        insert(&mut stale, managed_entry("CCC", "stale.jar"));

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
        assert!(loaded.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_reports_displaced_ownership_only_when_changed() {
        let mut manifest = ContentManifest::default();
        let old = managed_entry("AAA", "sodium-0.5.jar");
        assert_eq!(insert(&mut manifest, old.clone()), None);
        assert_eq!(
            insert(&mut manifest, managed_entry("AAA", "sodium-0.6.jar")),
            Some(old)
        );
        assert_eq!(
            insert(&mut manifest, managed_entry("AAA", "sodium-0.6.jar")),
            None
        );
        assert_eq!(manifest.len(), 1);
    }

    #[test]
    fn upsert_reports_displaced_ownership_when_kind_changes() {
        let mut manifest = ContentManifest::default();
        let previous = managed_entry("AAA", "shared.jar");
        insert(&mut manifest, previous.clone());
        let replacement = ManifestEntry::managed_file(
            previous.canonical_id().clone(),
            previous.provider(),
            previous.project_id().to_string(),
            previous.version_id().to_string(),
            ContentKind::ResourcePack,
            previous.managed_filename().expect("managed filename").clone(),
            previous.sha512().map(str::to_string),
            previous.size(),
            previous.dependencies().to_vec(),
            previous.title().map(str::to_string),
        )
        .expect("valid kind replacement");

        assert_eq!(insert(&mut manifest, replacement), Some(previous));
    }

    #[test]
    fn batch_upsert_is_atomic_when_a_later_entry_conflicts() {
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, managed_entry("AAA", "owned.jar"));
        let before = manifest.clone();

        let error = manifest.try_upsert_batch(vec![
            managed_entry("BBB", "new.jar"),
            managed_entry("CCC", "owned.jar"),
        ]);

        assert!(error.is_err());
        assert_eq!(manifest, before);
    }

    #[test]
    fn batch_upsert_rejects_an_oversized_input_before_mutation() {
        let mut manifest = ContentManifest::default();
        let before = manifest.clone();
        let repeated = managed_entry("oversized", "oversized.jar");

        let error = manifest.try_upsert_batch(vec![repeated; MAX_MANIFEST_ENTRIES + 1]);

        assert!(error.is_err());
        assert_eq!(manifest, before);
    }

    #[test]
    fn enabled_transition_is_atomic_at_the_serialized_bound() {
        let mut manifest = ContentManifest::default();
        manifest.entries = (0..MAX_MANIFEST_ENTRIES)
            .map(|index| {
                ManifestEntry::provenance(
                    CanonicalId::for_project(ProviderId::Modrinth, &format!("project-{index}")),
                    ProviderId::Modrinth,
                    format!("project-{index}"),
                    "version".to_string(),
                    None,
                )
                .expect("valid provenance entry")
            })
            .collect();

        let base_size = serde_json::to_vec_pretty(&manifest)
            .expect("serialize base manifest")
            .len();
        manifest.entries[0].title = Some(String::new());
        let empty_title_size = serde_json::to_vec_pretty(&manifest)
            .expect("serialize title field")
            .len();
        manifest.entries[0].title = None;
        let title_field_bytes = empty_title_size - base_size;
        let full_title_gain = title_field_bytes + MAX_TITLE_BYTES;
        let mut remaining = MAX_MANIFEST_BYTES - base_size;
        let mut index = 0;
        while remaining >= full_title_gain {
            manifest.entries[index].title = Some("x".repeat(MAX_TITLE_BYTES));
            remaining -= full_title_gain;
            index += 1;
        }
        if remaining > 0 && remaining < title_field_bytes {
            index -= 1;
            manifest.entries[index].title = Some(
                "x".repeat(MAX_TITLE_BYTES + remaining - title_field_bytes),
            );
            manifest.entries[index + 1].title = Some(String::new());
        } else if remaining > 0 {
            manifest.entries[index].title = Some("x".repeat(remaining - title_field_bytes));
        }

        manifest.validate().expect("valid bounded manifest");
        assert_eq!(
            serde_json::to_vec_pretty(&manifest)
                .expect("serialize bounded manifest")
                .len(),
            MAX_MANIFEST_BYTES
        );
        let before = manifest.clone();
        let canonical_id = manifest.entries[0].canonical_id().clone();

        assert!(manifest.try_set_enabled(&canonical_id, false).is_err());
        assert_eq!(manifest, before);
    }

    #[test]
    fn pending_projection_models_replacements_at_the_entry_limit() {
        let mut manifest = ContentManifest::default();
        manifest.entries = (0..MAX_MANIFEST_ENTRIES)
            .map(|index| {
                managed_entry(
                    &format!("project-{index}"),
                    &format!("managed-{index}.jar"),
                )
            })
            .collect();
        manifest.validate().expect("full manifest");
        let pending = PendingManifestEntry::managed_file(
            CanonicalId::for_project(ProviderId::Modrinth, "project-0"),
            ProviderId::Modrinth,
            "project-0".to_string(),
            "replacement".to_string(),
            ContentKind::Mod,
            ManagedContentFileName::new_exact("replacement.jar").expect("filename"),
            "a".repeat(128),
            Vec::new(),
            Some("Replacement".to_string()),
        )
        .expect("pending replacement");

        manifest
            .validate_provider_pending_projection(&[pending])
            .expect("replacement does not consume another manifest slot");
    }

    #[test]
    fn pending_projection_is_order_independent_when_ownership_moves() {
        let mut manifest = ContentManifest::default();
        manifest
            .try_upsert_batch(vec![
                managed_entry("first", "shared.jar"),
                managed_entry("second", "second.jar"),
            ])
            .expect("initial manifest");
        let pending = |project: &str, filename: &str| {
            PendingManifestEntry::managed_file(
                CanonicalId::for_project(ProviderId::Modrinth, project),
                ProviderId::Modrinth,
                project.to_string(),
                "replacement".to_string(),
                ContentKind::Mod,
                ManagedContentFileName::new_exact(filename).expect("filename"),
                "a".repeat(128),
                Vec::new(),
                None,
            )
            .expect("pending entry")
        };

        let forward = vec![
            pending("first", "first.jar"),
            pending("second", "shared.jar"),
        ];
        manifest
            .validate_provider_pending_projection(&forward)
            .expect("forward ownership transfer");
        let reverse = forward.into_iter().rev().collect::<Vec<_>>();
        manifest
            .validate_provider_pending_projection(&reverse)
            .expect("reverse ownership transfer");
    }

    #[test]
    fn pending_entries_validate_provider_fields_before_materialization() {
        let dependencies = (0..=MAX_ENTRY_DEPENDENCIES)
            .map(|index| ContentDependency {
                project_id: Some(format!("dependency-{index}")),
                version_id: None,
                kind: crate::model::DependencyKind::Required,
            })
            .collect();
        assert!(matches!(
            PendingManifestEntry::managed_file(
                CanonicalId::for_project(ProviderId::Modrinth, "project"),
                ProviderId::Modrinth,
                "project".to_string(),
                "version".to_string(),
                ContentKind::Mod,
                ManagedContentFileName::new_exact("managed.jar").expect("filename"),
                "a".repeat(128),
                dependencies,
                None,
            ),
            Err(ContentError::ProviderMetadataInvalid(_))
        ));
    }

    #[test]
    fn provider_entry_bounds_are_enforced_at_construction() {
        assert!(matches!(
            ManifestEntry::managed(
                CanonicalId::for_project(ProviderId::Modrinth, "AAA"),
                ProviderId::Modrinth,
                "AAA".to_string(),
                "AAA-v1".to_string(),
                ContentKind::Mod,
                &file_ref("managed.jar"),
                Vec::new(),
                Some("x".repeat(MAX_TITLE_BYTES + 1)),
            ),
            Err(ContentError::ProviderMetadataInvalid(_))
        ));
    }

    #[test]
    fn manifest_load_requires_the_exact_v3_schema_without_rewriting_rejections() {
        let dir = temp_game_dir("strict-schema");
        let path = manifest_path(&dir);
        let cases = [
            r#"{"entries":[]}"#,
            r#"{"schema_version":1,"entries":[]}"#,
            r#"{"schema_version":2,"entries":[]}"#,
            r#"{"schema_version":4,"entries":[]}"#,
            r#"{"schema_version":1,"entries":[],"unidentified":[]}"#,
            r#"{"schema_version":3,"entries":[],"extra":true}"#,
        ];

        for body in cases {
            fs::write(&path, body).expect("write malformed manifest");
            assert!(
                ContentManifest::load(&dir).is_err(),
                "manifest should fail closed: {body}"
            );
            assert_eq!(
                fs::read(&path).expect("read rejected manifest"),
                body.as_bytes(),
                "rejected manifests must remain byte-exact"
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

        value["entries"][0]["filename"] = serde_json::json!("");
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("legacy empty filename body"),
        )
        .expect("write legacy empty filename");
        assert!(ContentManifest::load(&dir).is_err());
        value["entries"][0]["filename"] = serde_json::json!("sodium.jar");

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
        value["entries"][0]["sha1"] = serde_json::json!("0".repeat(40));
        let legacy_sha1 = serde_json::to_vec(&value).expect("legacy sha1 body");
        fs::write(&path, &legacy_sha1).expect("write legacy sha1");
        assert!(ContentManifest::load(&dir).is_err());
        assert_eq!(fs::read(&path).expect("read legacy sha1"), legacy_sha1);

        value["entries"][0]
            .as_object_mut()
            .expect("entry object")
            .remove("sha1");
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
        let duplicate_id = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "AAA"),
            ProviderId::Modrinth,
            "AAA".to_string(),
            "AAA-v2".to_string(),
            ContentKind::Mod,
            &file_ref("second.jar"),
            Vec::new(),
            Some("AAA".to_string()),
        )
        .expect("valid duplicate-id fixture");
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

        let unicode_duplicate_files = ContentManifest {
            entries: vec![
                managed_entry("CCC", "Stra\u{df}e.jar"),
                managed_entry("DDD", "STRASSE.jar"),
            ],
            ..ContentManifest::default()
        };
        fs::write(
            &path,
            serde_json::to_vec(&unicode_duplicate_files).expect("Unicode duplicate body"),
        )
        .expect("write Unicode duplicates");
        assert!(ContentManifest::load(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_unbounded_or_unsafe_entries() {
        let dir = temp_game_dir("bounded-entry");
        let path = manifest_path(&dir);
        let entry = managed_entry("AAA", "tracked.jar");
        let mut body = serde_json::to_value(ContentManifest {
            entries: vec![entry],
            ..ContentManifest::default()
        })
        .expect("oversized entry body");
        body["entries"][0]["filename"] = serde_json::Value::String("x".repeat(
            axial_minecraft::portable_path::MAX_PORTABLE_FILE_NAME_BYTES + 1,
        ));
        fs::write(
            &path,
            serde_json::to_vec(&body).expect("oversized entry body"),
        )
        .expect("write oversized entry");
        assert!(ContentManifest::load(&dir).is_err());

        let invalid_body = |filename: &str| {
            let mut invalid = body.clone();
            invalid["entries"][0]["filename"] =
                serde_json::Value::String(filename.to_string());
            serde_json::to_vec(&invalid).expect("invalid filename body")
        };
        fs::write(
            &path,
            invalid_body("../tracked.jar"),
        )
        .expect("write unsafe entry");
        assert!(ContentManifest::load(&dir).is_err());

        fs::write(&path, invalid_body("tracked.jar.DISABLED"))
            .expect("write disabled base filename");
        assert!(ContentManifest::load(&dir).is_err());

        for filename in ["Cafe\u{301}.jar", ".axial-pack-staging.jar"] {
            fs::write(&path, invalid_body(filename)).expect("write non-portable filename");
            assert!(ContentManifest::load(&dir).is_err(), "accepted {filename:?}");
        }

        fs::write(&path, vec![b' '; MAX_MANIFEST_BYTES + 1]).expect("oversized manifest");
        assert!(ContentManifest::load(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn p00_b11_contract_file_owners_require_canonical_sha512_and_positive_exact_size() {
        let dir = temp_game_dir("checksum-required");
        let path = manifest_path(&dir);
        let entry = managed_entry("AAA", "tracked.jar");
        let mut wire = serde_json::to_value(ContentManifest {
            entries: vec![entry.clone()],
            ..ContentManifest::default()
        })
        .expect("manifest body");
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!("A".repeat(128)),
        ] {
            wire["entries"][0]["sha512"] = invalid;
            fs::write(&path, serde_json::to_vec(&wire).expect("invalid hash body"))
                .expect("write manifest");
            assert!(ContentManifest::load(&dir).is_err());
        }
        wire["entries"][0]["sha512"] = serde_json::json!("a".repeat(128));
        for invalid in [serde_json::Value::Null, serde_json::json!(0)] {
            wire["entries"][0]["size"] = invalid;
            fs::write(&path, serde_json::to_vec(&wire).expect("invalid size body"))
                .expect("write manifest");
            assert!(ContentManifest::load(&dir).is_err());
        }
        let tracked = dir.join("mods").join("tracked.jar");
        fs::create_dir_all(tracked.parent().expect("mods dir")).expect("create mods");
        fs::write(&tracked, b"unverified").expect("write unverified file");
        assert!(!entry_path_matches(&tracked, &entry));

        fs::remove_file(&path).expect("remove rejected manifest");
        let entry = ManifestEntry::provenance(
            CanonicalId::for_project(ProviderId::Modrinth, "AAA"),
            ProviderId::Modrinth,
            "AAA".to_string(),
            "AAA-v1".to_string(),
            Some("AAA".to_string()),
        )
        .expect("valid provenance entry");
        assert!(
            serde_json::to_value(&entry)
                .expect("provenance JSON")
                .get("filename")
                .is_none(),
            "provenance must not encode a sentinel filename"
        );
        let mut manifest = ContentManifest::default();
        insert(&mut manifest, entry);
        manifest
            .save(&dir)
            .expect("save provenance-only pack entry");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn p00_b11_contract_entry_presence_rejects_same_size_corruption() {
        let dir = temp_game_dir("entry-presence");
        let mods_dir = dir.join("mods");
        fs::create_dir_all(&mods_dir).expect("mods dir");
        let path = mods_dir.join("tracked.jar");
        fs::write(&path, b"old").expect("tracked file");

        let mut entry = managed_entry("AAA", "tracked.jar");
        entry
            .record_authenticated_file(3, sha512_file(&path).expect("tracked hash"))
            .expect("record authenticated file");
        assert!(entry_file_present(&dir, &entry));

        fs::write(&path, b"new").expect("replace tracked file");
        assert!(!entry_file_present(&dir, &entry));

        fs::write(&path, b"old").expect("restore tracked file");
        fs::rename(&path, mods_dir.join("tracked.jar.disabled")).expect("disable");
        assert!(entry_file_present(&dir, &entry));
        fs::remove_dir_all(&dir).ok();
    }
}
