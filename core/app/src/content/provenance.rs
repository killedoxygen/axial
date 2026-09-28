//! Strict content ownership records. Decoding metadata never grants file authority.

use super::catalog::{ContentError, ContentResult};
use super::model::{CanonicalId, ContentDependency, ContentKind, FileRef, ProviderId};
use crate::files::portable::{
    PortableFileName, PortablePathError, PortablePathKey, managed_content_name_is_reserved,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub const MANIFEST_FILE: &str = "axial.content.json";
pub const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 4096;

/// Validate predecessor metadata without granting live ownership or rewriting
/// the original bytes retained by the instance publisher.
pub(crate) fn validate_legacy_manifest(bytes: &[u8]) -> ContentResult<()> {
    // Decode the original bytes first, preserving strict duplicate-field checks
    // and shared bounds. The predecessor never emitted pack handoff evidence.
    ContentManifest::decode_managed(Some(bytes))?;
    let source: serde_json::Value = serde_json::from_slice(bytes)?;
    if source["entries"].as_array().is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| entry.get("pack_installation").is_some())
    }) {
        return Err(invalid("unsupported legacy content manifest field"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ManagedContentFileName {
    enabled: PortableFileName,
    disabled: PortableFileName,
}

impl ManagedContentFileName {
    pub(crate) fn new_exact(value: &str) -> Result<Self, PortablePathError> {
        let enabled = PortableFileName::new_exact(value)?;
        if enabled.key().as_str().ends_with(".disabled")
            || managed_content_name_is_reserved(&enabled)
        {
            return Err(PortablePathError::Unsafe);
        }
        let disabled = enabled.with_suffix(".disabled")?;
        Ok(Self { enabled, disabled })
    }
    pub fn as_str(&self) -> &str {
        self.enabled.as_str()
    }
    pub(crate) fn key(&self) -> PortablePathKey {
        self.enabled.key()
    }
    pub(crate) fn disabled(&self) -> &PortableFileName {
        &self.disabled
    }
}

impl Serialize for ManagedContentFileName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for ManagedContentFileName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new_exact(&String::deserialize(deserializer)?)
            .map_err(|_| de::Error::custom("invalid managed content filename"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    canonical_id: CanonicalId,
    provider: ProviderId,
    project_id: String,
    version_id: String,
    kind: ContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<ManagedContentFileName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha512: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, deserialize_with = "strict_dependencies")]
    dependencies: Vec<ContentDependency>,
    enabled: bool,
    installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_installation: Option<PackInstallation>,
}

/// Evidence for an idempotent creation handoff, not permission to overwrite or
/// remove these paths. Pack removal continues to remove provenance only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackInstallation {
    pub fingerprint: String,
    pub files: Vec<PackInstalledFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackInstalledFile {
    pub path: String,
    pub size: u64,
    pub sha512: String,
}

impl PackInstallation {
    fn validate(&self) -> ContentResult<()> {
        if self.fingerprint.len() != 64
            || !self
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.files.len() > 20_000
            || self.files.iter().any(|file| {
                file.size > super::packs::MAX_PACK_FILE_BYTES || !valid_sha512(&file.sha512)
            })
        {
            return Err(invalid("invalid pack installation evidence"));
        }
        super::packs::validate_destinations(self.files.iter().map(|file| file.path.as_str()))
            .map_err(|_| invalid("invalid pack installation destinations"))
    }
}

fn strict_dependencies<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ContentDependency>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        #[serde(default)]
        project_id: Option<String>,
        #[serde(default)]
        version_id: Option<String>,
        kind: super::model::DependencyKind,
    }
    Vec::<Wire>::deserialize(deserializer).map(|values| {
        values
            .into_iter()
            .map(|value| ContentDependency {
                project_id: value.project_id,
                version_id: value.version_id,
                kind: value.kind,
            })
            .collect()
    })
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
        let filename = ManagedContentFileName::new_exact(&file.filename)
            .map_err(|_| invalid("invalid content filename"))?;
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
            installed_at: chrono::Utc::now().to_rfc3339(),
            title,
            pack_installation: None,
        };
        entry.validate().map_err(|_| {
            ContentError::ProviderMetadataInvalid("invalid content ownership metadata".into())
        })?;
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
            installed_at: chrono::Utc::now().to_rfc3339(),
            title,
            pack_installation: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn canonical_id(&self) -> &CanonicalId {
        &self.canonical_id
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

    pub(crate) fn pack_installation(&self) -> Option<&PackInstallation> {
        self.pack_installation.as_ref()
    }

    pub(crate) fn record_pack_installation(
        &mut self,
        proof: PackInstallation,
    ) -> ContentResult<()> {
        let mut candidate = self.clone();
        candidate.pack_installation = Some(proof);
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub fn record_authenticated_file(&mut self, size: u64, sha512: String) -> ContentResult<()> {
        let mut candidate = self.clone();
        candidate.size = Some(size);
        candidate.sha512 = Some(sha512);
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    fn validate(&self) -> ContentResult<()> {
        for value in [
            self.canonical_id.as_str(),
            &self.project_id,
            &self.version_id,
        ] {
            validate_text(value, 512)?;
        }
        if self.canonical_id != CanonicalId::for_project(self.provider, &self.project_id) {
            return Err(invalid(
                "content canonical identity disagrees with provider project",
            ));
        }
        if self.kind == ContentKind::Modpack {
            if self.filename.is_some() || self.sha512.is_some() || self.size.is_some() {
                return Err(invalid("modpack provenance cannot own a file"));
            }
            if let Some(proof) = &self.pack_installation {
                proof.validate()?;
            }
        } else {
            if self.pack_installation.is_some() {
                return Err(invalid(
                    "only modpacks can record pack installation evidence",
                ));
            }
            let filename = self
                .filename
                .as_ref()
                .ok_or_else(|| invalid("missing content filename"))?;
            if self.kind == ContentKind::Mod && !filename.key().as_str().ends_with(".jar") {
                return Err(invalid("managed mod filename must end in .jar"));
            }
            if !self.sha512.as_deref().is_some_and(valid_sha512)
                || self.size.is_none_or(|size| size == 0)
            {
                return Err(invalid(
                    "content ownership requires SHA-512 and an exact positive size",
                ));
            }
        }
        if self.dependencies.len() > 256 {
            return Err(invalid("too many content dependencies"));
        }
        for dependency in &self.dependencies {
            if dependency.project_id.is_none() && dependency.version_id.is_none() {
                return Err(invalid("content dependency has no identity"));
            }
            for value in [
                dependency.project_id.as_deref(),
                dependency.version_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                validate_text(value, 512)?;
            }
        }
        validate_text(&self.installed_at, 64)?;
        if chrono::DateTime::parse_from_rfc3339(&self.installed_at).is_err() {
            return Err(invalid("invalid content installation timestamp"));
        }
        if self.title.as_ref().is_some_and(|title| title.len() > 1024) {
            return Err(invalid("content title exceeds its bound"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContentManifest {
    schema_version: u32,
    entries: Vec<ManifestEntry>,
}

impl Default for ContentManifest {
    fn default() -> Self {
        Self {
            schema_version: 3,
            entries: Vec::new(),
        }
    }
}

impl ContentManifest {
    pub fn decode_managed(bytes: Option<&[u8]>) -> ContentResult<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u32,
            entries: Vec<ManifestEntry>,
        }
        let Some(bytes) = bytes else {
            return Ok(Self::default());
        };
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("content manifest exceeds its byte bound"));
        }
        let wire: Wire = serde_json::from_slice(bytes)?;
        let manifest = Self {
            schema_version: wire.schema_version,
            entries: wire.entries,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn encode_managed(&self) -> ContentResult<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("content manifest exceeds its byte bound"));
        }
        Ok(bytes)
    }

    pub fn fingerprint(&self) -> ContentResult<String> {
        Ok(hex::encode(Sha256::digest(self.encode_managed()?)))
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
    pub fn find(&self, id: &CanonicalId) -> Option<&ManifestEntry> {
        self.entries.iter().find(|entry| &entry.canonical_id == id)
    }

    pub fn try_upsert(&mut self, entry: ManifestEntry) -> ContentResult<Option<ManifestEntry>> {
        Ok(self.try_upsert_batch(vec![entry])?.into_iter().next())
    }

    /// Validate the entire replacement before changing even the first entry.
    pub fn try_upsert_batch(
        &mut self,
        additions: Vec<ManifestEntry>,
    ) -> ContentResult<Vec<ManifestEntry>> {
        if additions.len() > MAX_MANIFEST_ENTRIES {
            return Err(invalid("too many content entries"));
        }
        let mut ids = HashSet::new();
        let mut candidate = self.clone();
        let mut displaced = Vec::new();
        for entry in additions {
            if !ids.insert(entry.canonical_id.clone()) {
                return Err(invalid("duplicate content identity"));
            }
            if let Some(index) = candidate
                .entries
                .iter()
                .position(|old| old.canonical_id == entry.canonical_id)
            {
                let old = std::mem::replace(&mut candidate.entries[index], entry);
                if old.filename != candidate.entries[index].filename
                    || old.kind != candidate.entries[index].kind
                {
                    displaced.push(old);
                }
            } else {
                candidate.entries.push(entry);
            }
        }
        candidate.encode_managed()?;
        *self = candidate;
        Ok(displaced)
    }

    pub fn remove(&mut self, id: &CanonicalId) -> Option<ManifestEntry> {
        self.entries
            .iter()
            .position(|entry| &entry.canonical_id == id)
            .map(|index| self.entries.remove(index))
    }

    pub fn try_set_enabled(
        &mut self,
        id: &CanonicalId,
        enabled: bool,
    ) -> ContentResult<Option<bool>> {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| &entry.canonical_id == id)
        else {
            return Ok(None);
        };
        if self.entries[index].enabled == enabled {
            return Ok(Some(false));
        }
        let mut candidate = self.clone();
        candidate.entries[index].enabled = enabled;
        candidate.encode_managed()?;
        *self = candidate;
        Ok(Some(true))
    }

    pub fn validate_provider_entry(&self, entry: &ManifestEntry) -> ContentResult<()> {
        entry.validate().map_err(|_| {
            ContentError::ProviderMetadataInvalid("invalid provider content entry".into())
        })
    }

    pub fn validate_provider_projection(&self) -> ContentResult<()> {
        self.encode_managed().map(|_| ()).map_err(|_| {
            ContentError::ProviderMetadataInvalid("provider content exceeds manifest bounds".into())
        })
    }

    fn validate(&self) -> ContentResult<()> {
        if self.schema_version != 3 || self.entries.len() > MAX_MANIFEST_ENTRIES {
            return Err(invalid("unsupported or oversized content manifest"));
        }
        let mut identities = HashSet::new();
        let mut names = HashSet::new();
        for entry in &self.entries {
            entry.validate()?;
            if !identities.insert(entry.canonical_id.clone()) {
                return Err(invalid("duplicate content identity"));
            }
            if let Some(filename) = entry.managed_filename() {
                if !names.insert((entry.kind, filename.key())) {
                    return Err(invalid("duplicate portable content path"));
                }
            }
        }
        Ok(())
    }
}

/// Only authenticated observations enter this resolver input; metadata alone is insufficient.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveManagedContent {
    entries: HashMap<CanonicalId, ManifestEntry>,
}
impl LiveManagedContent {
    pub fn contains(&self, entry: &ManifestEntry) -> bool {
        self.entries.get(entry.canonical_id()) == Some(entry)
    }
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a ManifestEntry>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| (entry.canonical_id.clone(), entry.clone()))
                .collect(),
        }
    }
}

pub(crate) fn valid_sha512(value: &str) -> bool {
    value.len() == 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn validate_text(value: &str, maximum: usize) -> ContentResult<()> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(invalid("invalid content identity text"));
    }
    Ok(())
}
fn invalid(message: &str) -> ContentError {
    ContentError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_manifest() -> serde_json::Value {
        let mut entries = Vec::new();
        for (project, kind, filename) in [
            ("mod", "mod", "old.jar"),
            ("resources", "resource_pack", "old-resources.zip"),
            ("shaders", "shader_pack", "old-shaders.zip"),
        ] {
            entries.push(serde_json::json!({
                "canonical_id": format!("modrinth:{project}"),
                "provider": "modrinth",
                "project_id": project,
                "version_id": "old-version",
                "kind": kind,
                "filename": filename,
                "sha512": "a".repeat(128),
                "size": 3,
                "dependencies": [{"project_id":"dependency", "kind":"optional"}],
                "enabled": false,
                "installed_at": "2024-02-29T12:34:56+02:00",
                "title": "Historical title"
            }));
        }
        entries.push(serde_json::json!({
            "canonical_id": "modrinth:pack",
            "provider": "modrinth",
            "project_id": "pack",
            "version_id": "old-pack-version",
            "kind": "modpack",
            "dependencies": [],
            "enabled": true,
            "installed_at": "2024-02-29T12:34:56+02:00",
            "title": "Historical pack"
        }));
        serde_json::json!({"schema_version":3, "entries":entries})
    }

    #[test]
    fn legacy_manifest_preserves_historical_metadata_without_live_authority() {
        let bytes = serde_json::to_vec(&legacy_manifest()).unwrap();
        validate_legacy_manifest(&bytes).unwrap();
        let manifest = ContentManifest::decode_managed(Some(&bytes)).unwrap();
        assert_eq!(manifest.len(), 4);
        for entry in &manifest.entries()[..3] {
            assert!(!entry.enabled());
            assert_eq!(entry.installed_at(), "2024-02-29T12:34:56+02:00");
            assert_eq!(entry.title(), Some("Historical title"));
            assert_eq!(
                entry.dependencies()[0].project_id.as_deref(),
                Some("dependency")
            );
            assert!(!LiveManagedContent::default().contains(entry));
        }
        let pack = &manifest.entries()[3];
        assert_eq!(pack.kind(), ContentKind::Modpack);
        assert!(pack.managed_filename().is_none());
        assert!(pack.pack_installation().is_none());
    }

    #[test]
    fn legacy_manifest_rejects_replacement_only_evidence_even_when_null() {
        for evidence in [
            serde_json::Value::Null,
            serde_json::json!({"fingerprint":"a".repeat(64), "files":[]}),
        ] {
            let mut source = legacy_manifest();
            source["entries"][3]["pack_installation"] = evidence;
            let bytes = serde_json::to_vec(&source).unwrap();
            assert!(ContentManifest::decode_managed(Some(&bytes)).is_ok());
            assert!(validate_legacy_manifest(&bytes).is_err());
        }
    }

    #[test]
    fn legacy_manifest_keeps_strict_shared_validation_and_bounds() {
        for (pointer, value) in [
            ("/schema_version", serde_json::json!(2)),
            (
                "/entries/0/canonical_id",
                serde_json::json!("modrinth:other"),
            ),
            ("/entries/0/sha512", serde_json::json!("invalid")),
            ("/entries/0/size", serde_json::json!(0)),
            (
                "/entries/0/dependencies",
                serde_json::json!([{"kind":"optional"}]),
            ),
            (
                "/entries/0/installed_at",
                serde_json::json!("2024-02-30T12:34:56Z"),
            ),
            ("/entries/0/filename", serde_json::json!("../outside.jar")),
            // The old decoder accepted hand-edited non-JAR mod entries, but
            // its actual producers and the replacement owner do not.
            ("/entries/0/filename", serde_json::json!("unsupported.zip")),
        ] {
            let mut source = legacy_manifest();
            *source.pointer_mut(pointer).unwrap() = value;
            assert!(validate_legacy_manifest(&serde_json::to_vec(&source).unwrap()).is_err());
        }
        let mut source = legacy_manifest();
        source["entries"][0]["unknown"] = true.into();
        assert!(validate_legacy_manifest(&serde_json::to_vec(&source).unwrap()).is_err());
        let mut source = legacy_manifest();
        let mut alias = source["entries"][0].clone();
        alias["canonical_id"] = "modrinth:alias".into();
        alias["project_id"] = "alias".into();
        alias["filename"] = "OLD.JAR".into();
        source["entries"].as_array_mut().unwrap().push(alias);
        assert!(validate_legacy_manifest(&serde_json::to_vec(&source).unwrap()).is_err());
        for bytes in [
            br#"{"schema_version":3,"schema_version":3,"entries":[]}"#.as_slice(),
            br#"{"schema_version":3,"entries":[],"entries":[]}"#.as_slice(),
        ] {
            assert!(validate_legacy_manifest(bytes).is_err());
        }
        let mut bytes = br#"{"schema_version":3,"entries":[]}"#.to_vec();
        bytes.resize(MAX_MANIFEST_BYTES, b' ');
        validate_legacy_manifest(&bytes).unwrap();
        bytes.push(b' ');
        assert!(validate_legacy_manifest(&bytes).is_err());
    }

    #[test]
    fn legacy_manifest_enforces_the_entry_bound() {
        let mut source = legacy_manifest();
        let pack = source["entries"][3].clone();
        let entries = source["entries"].as_array_mut().unwrap();
        entries.clear();
        for index in 0..MAX_MANIFEST_ENTRIES {
            let mut entry = pack.clone();
            entry["canonical_id"] = format!("modrinth:pack-{index}").into();
            entry["project_id"] = format!("pack-{index}").into();
            entries.push(entry);
        }
        validate_legacy_manifest(&serde_json::to_vec(&source).unwrap()).unwrap();
        source["entries"].as_array_mut().unwrap().push(pack);
        assert!(validate_legacy_manifest(&serde_json::to_vec(&source).unwrap()).is_err());
    }

    fn entry(project: &str, filename: &str) -> ManifestEntry {
        ManifestEntry::managed_file(
            CanonicalId::for_project(ProviderId::Modrinth, project),
            ProviderId::Modrinth,
            project.into(),
            "version".into(),
            ContentKind::Mod,
            ManagedContentFileName::new_exact(filename).unwrap(),
            Some("a".repeat(128)),
            Some(3),
            Vec::new(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn rejects_unknown_schema_fields_and_unverified_ownership() {
        for json in [
            r#"{"schema_version":4,"entries":[]}"#,
            r#"{"schema_version":3,"entries":[],"extra":true}"#,
        ] {
            assert!(ContentManifest::decode_managed(Some(json.as_bytes())).is_err());
        }
        let mut manifest = ContentManifest::default();
        manifest.try_upsert(entry("project", "file.jar")).unwrap();
        let mut wire = serde_json::to_value(&manifest).unwrap();
        wire["entries"][0]["sha512"] = serde_json::Value::Null;
        assert!(
            ContentManifest::decode_managed(Some(&serde_json::to_vec(&wire).unwrap())).is_err()
        );
    }

    #[test]
    fn pack_installation_evidence_is_optional_and_rejects_unsafe_destinations() {
        let mut pack = ManifestEntry::provenance(
            CanonicalId::for_project(ProviderId::Modrinth, "pack"),
            ProviderId::Modrinth,
            "pack".into(),
            "version".into(),
            None,
        )
        .unwrap();
        let mut manifest = ContentManifest::default();
        manifest.try_upsert(pack.clone()).unwrap();
        let old_bytes = manifest.encode_managed().unwrap();
        assert!(!String::from_utf8_lossy(&old_bytes).contains("pack_installation"));
        assert_eq!(
            ContentManifest::decode_managed(Some(&old_bytes)).unwrap(),
            manifest
        );
        let proof = PackInstallation {
            fingerprint: "a".repeat(64),
            files: vec![PackInstalledFile {
                path: "config/nested/empty.txt".into(),
                size: 0,
                sha512: "b".repeat(128),
            }],
        };
        pack.record_pack_installation(proof.clone()).unwrap();
        manifest.try_upsert(pack).unwrap();
        assert_eq!(
            ContentManifest::decode_managed(Some(&manifest.encode_managed().unwrap())).unwrap(),
            manifest
        );
        let mut wire = serde_json::to_value(&manifest).unwrap();
        for path in ["../outside", "axial.content.json", "Mods/data.jar"] {
            wire["entries"][0]["pack_installation"]["files"][0]["path"] = path.into();
            assert!(
                ContentManifest::decode_managed(Some(&serde_json::to_vec(&wire).unwrap())).is_err()
            );
        }
        assert!(
            entry("mod", "mod.jar")
                .record_pack_installation(proof)
                .is_err()
        );
    }

    #[test]
    fn batch_collision_is_atomic_including_unicode_and_disabled_aliases() {
        let mut manifest = ContentManifest::default();
        manifest.try_upsert(entry("project", "Straße.jar")).unwrap();
        let before = manifest.clone();
        assert!(
            manifest
                .try_upsert_batch(vec![
                    entry("new", "okay.jar"),
                    entry("collision", "STRASSE.jar")
                ])
                .is_err()
        );
        assert_eq!(manifest, before);
        assert!(ManagedContentFileName::new_exact("x.jar.DISABLED").is_err());
        assert!(ManagedContentFileName::new_exact("Cafe\u{301}.jar").is_err());
    }

    #[test]
    fn metadata_does_not_establish_liveness_and_pack_has_no_sentinel_path() {
        let record = entry("project", "file.jar");
        assert!(!LiveManagedContent::default().contains(&record));
        assert!(LiveManagedContent::from_entries([&record]).contains(&record));
        let pack = ManifestEntry::provenance(
            CanonicalId::for_project(ProviderId::Modrinth, "pack"),
            ProviderId::Modrinth,
            "pack".into(),
            "version".into(),
            None,
        )
        .unwrap();
        assert!(
            serde_json::to_value(pack)
                .unwrap()
                .get("filename")
                .is_none()
        );
    }
}
