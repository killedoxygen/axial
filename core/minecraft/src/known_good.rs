use crate::artifact_path::{
    ArtifactRelativePath, MAX_ARTIFACT_PATH_SEGMENT_BYTES, MAX_ARTIFACT_RELATIVE_PATH_BYTES,
};
use crate::download::{
    ExpectedIntegrity, LibraryArtifactPlan, LibraryChecksumPolicy, library_artifact_plans_for,
    parse_asset_index,
};
use crate::launch::{Library, VersionJson};
use crate::loaders::{
    LoaderBuildRecord, LoaderComponentId, LoaderInstallStrategy, installed_loader_metadata_bytes,
};
use crate::manifest::ManifestEntry;
use crate::rules::Environment;
use crate::runtime::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, RuntimeId, component_manifest_proof_bytes,
    plan_runtime_manifest_files, preferred_runtime_component,
};
use sha1::{Digest as _, Sha1};
use std::collections::BTreeMap;
use std::path::{Component, Path};

pub const MAX_KNOWN_GOOD_RELATIVE_PATH_BYTES: usize = MAX_ARTIFACT_RELATIVE_PATH_BYTES;
pub const MAX_KNOWN_GOOD_PATH_SEGMENT_BYTES: usize = MAX_ARTIFACT_PATH_SEGMENT_BYTES;
pub const MAX_KNOWN_GOOD_ENTRIES: usize = 200_000;
pub const MAX_KNOWN_GOOD_ASSET_INDEX_BYTES: usize = 64 << 20;
pub const MAX_KNOWN_GOOD_RUNTIME_MANIFEST_BYTES: usize = 16 << 20;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum KnownGoodRoot {
    Versions,
    Libraries,
    Assets,
    ManagedRuntime { component: KnownGoodId },
}

impl KnownGoodRoot {
    pub fn stable_id(&self) -> &'static str {
        match self {
            Self::Versions => "versions",
            Self::Libraries => "libraries",
            Self::Assets => "assets",
            Self::ManagedRuntime { .. } => "managed_runtime",
        }
    }

    pub fn scope_id(&self) -> &str {
        match self {
            Self::ManagedRuntime { component } => component.as_str(),
            _ => "",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnownGoodArtifactKind {
    VersionMetadata,
    ClientJar,
    Library,
    NativeLibrary,
    AssetIndex,
    AssetObject,
    LogConfig,
    LoaderMetadata,
    RuntimeManifestProof,
    RuntimeReadyMarker,
    RuntimeFile,
    RuntimeExecutable,
    RuntimeDirectory,
    RuntimeLink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KnownGoodIntegrity {
    Sha1 {
        digest: Sha1Digest,
        size: Option<u64>,
    },
    StructuralJar {
        size: Option<u64>,
    },
    ExactBytes {
        digest: Sha1Digest,
        size: u64,
    },
    Directory,
    LinkTarget(KnownGoodLinkTarget),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownGoodEntry {
    root: KnownGoodRoot,
    path: KnownGoodRelativePath,
    kind: KnownGoodArtifactKind,
    integrity: KnownGoodIntegrity,
}

impl KnownGoodEntry {
    pub fn root(&self) -> &KnownGoodRoot {
        &self.root
    }

    pub fn path(&self) -> &KnownGoodRelativePath {
        &self.path
    }

    pub fn kind(&self) -> KnownGoodArtifactKind {
        self.kind
    }

    pub fn integrity(&self) -> &KnownGoodIntegrity {
        &self.integrity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownGoodInventory {
    entries: Vec<KnownGoodEntry>,
}

impl KnownGoodInventory {
    pub fn entries(&self) -> &[KnownGoodEntry] {
        &self.entries
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KnownGoodRelativePath(String);

impl KnownGoodRelativePath {
    pub fn new(value: &str) -> Result<Self, KnownGoodInventoryError> {
        ArtifactRelativePath::new(value)
            .map(|path| Self(path.as_str().to_string()))
            .map_err(|_| KnownGoodInventoryError::UnsafePath)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct KnownGoodId(String);

impl KnownGoodId {
    fn new(value: &str) -> Result<Self, KnownGoodInventoryError> {
        let path = KnownGoodRelativePath::new(value)?;
        if path.as_str().contains('/') {
            return Err(KnownGoodInventoryError::UnsafePath);
        }
        Ok(Self(path.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sha1Digest(String);

impl Sha1Digest {
    fn from_metadata(value: &str) -> Result<Self, KnownGoodInventoryError> {
        let value = value.trim();
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(KnownGoodInventoryError::InvalidSha1);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownGoodLinkTarget(String);

impl KnownGoodLinkTarget {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimeInventoryInput<'a> {
    pub(crate) component: &'a RuntimeId,
    pub(crate) manifest_bytes: &'a [u8],
    pub(crate) manifest_expected: &'a ExpectedIntegrity,
}

#[derive(Clone, Copy)]
pub enum KnownGoodInstallShape<'a> {
    Vanilla {
        version_manifest: &'a ManifestEntry,
    },
    Fabric {
        record: &'a LoaderBuildRecord,
    },
    Quilt {
        record: &'a LoaderBuildRecord,
    },
    Forge {
        record: &'a LoaderBuildRecord,
        installer_libraries: &'a [Library],
    },
    NeoForge {
        record: &'a LoaderBuildRecord,
        installer_libraries: &'a [Library],
    },
}

impl<'a> KnownGoodInstallShape<'a> {
    fn loader_record(&self) -> Option<&'a LoaderBuildRecord> {
        match self {
            Self::Vanilla { .. } => None,
            Self::Fabric { record }
            | Self::Quilt { record }
            | Self::Forge { record, .. }
            | Self::NeoForge { record, .. } => Some(record),
        }
    }

    fn installer_libraries(&self) -> &'a [Library] {
        match self {
            Self::Forge {
                installer_libraries,
                ..
            }
            | Self::NeoForge {
                installer_libraries,
                ..
            } => installer_libraries,
            _ => &[],
        }
    }

    fn component(&self) -> Option<LoaderComponentId> {
        match self {
            Self::Vanilla { .. } => None,
            Self::Fabric { .. } => Some(LoaderComponentId::Fabric),
            Self::Quilt { .. } => Some(LoaderComponentId::Quilt),
            Self::Forge { .. } => Some(LoaderComponentId::Forge),
            Self::NeoForge { .. } => Some(LoaderComponentId::NeoForge),
        }
    }
}

pub struct KnownGoodInventoryInput<'a> {
    pub(crate) resolved_version: &'a VersionJson,
    pub(crate) asset_index_bytes: Option<&'a [u8]>,
    pub(crate) runtime: Option<RuntimeInventoryInput<'a>>,
    pub(crate) shape: KnownGoodInstallShape<'a>,
    pub(crate) environment: &'a Environment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KnownGoodInventoryError {
    UnsafePath,
    InvalidSha1,
    MissingChecksum,
    MissingAssetIndex,
    UnexpectedAssetIndex,
    AssetIndexIntegrity,
    AssetIndexParse,
    InvalidAssetObject,
    UnsupportedRuntimeEntry,
    MissingRuntimeDownload,
    RuntimeManifestParse,
    RuntimeManifestIntegrity,
    VersionIdentityMismatch,
    LoaderIdentityMismatch,
    RuntimeIdentityMismatch,
    MetadataSerialization,
    MissingClient,
    InputTooLarge,
    InvalidLibraryPlan,
    ConflictingEntry,
    TooManyEntries,
}

pub fn derive_known_good_inventory(
    input: KnownGoodInventoryInput<'_>,
) -> Result<KnownGoodInventory, KnownGoodInventoryError> {
    let version_id = KnownGoodId::new(&input.resolved_version.id)?;
    let mut builder = InventoryBuilder::default();
    let version_base = version_id.as_str();

    builder.insert(KnownGoodEntry {
        root: KnownGoodRoot::Versions,
        path: KnownGoodRelativePath::new(&format!("{version_base}/{version_base}.json"))?,
        kind: KnownGoodArtifactKind::VersionMetadata,
        integrity: version_metadata_integrity(input.shape, input.resolved_version)?,
    })?;

    let client = input
        .resolved_version
        .downloads
        .client
        .as_ref()
        .ok_or(KnownGoodInventoryError::MissingClient)?;
    builder.insert(KnownGoodEntry {
        root: KnownGoodRoot::Versions,
        path: KnownGoodRelativePath::new(&format!("{version_base}/{version_base}.jar"))?,
        kind: KnownGoodArtifactKind::ClientJar,
        integrity: expected_integrity(
            &ExpectedIntegrity::from_mojang(client.size, &client.sha1),
            false,
            &format!("{version_base}.jar"),
        )?,
    })?;

    let checksum_policy = if input.shape.loader_record().is_some() {
        LibraryChecksumPolicy::AllowMissing
    } else {
        LibraryChecksumPolicy::Strict
    };
    if input
        .resolved_version
        .libraries
        .len()
        .saturating_add(input.shape.installer_libraries().len())
        > MAX_KNOWN_GOOD_ENTRIES
    {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    let libraries = library_artifact_plans_for(
        &input.resolved_version.libraries,
        input.environment,
        checksum_policy,
    )
    .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    add_library_plans(&mut builder, libraries)?;
    let installer_libraries = library_artifact_plans_for(
        input.shape.installer_libraries(),
        input.environment,
        checksum_policy,
    )
    .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    add_library_plans(&mut builder, installer_libraries)?;

    if let Some(logging) = input
        .resolved_version
        .logging
        .as_ref()
        .and_then(|logging| logging.client.as_ref())
        && !logging.file.url.trim().is_empty()
    {
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Assets,
            path: KnownGoodRelativePath::new(&format!("log_configs/{}", logging.file.id))?,
            kind: KnownGoodArtifactKind::LogConfig,
            integrity: expected_integrity(
                &ExpectedIntegrity::from_mojang(logging.file.size, &logging.file.sha1),
                false,
                &logging.file.id,
            )?,
        })?;
    }

    add_asset_index(
        &mut builder,
        input.resolved_version,
        input.asset_index_bytes,
    )?;
    if let Some(runtime) = input.runtime {
        if runtime.component.as_str()
            != preferred_runtime_component(&input.resolved_version.java_version)
        {
            return Err(KnownGoodInventoryError::RuntimeIdentityMismatch);
        }
        add_runtime(&mut builder, runtime)?;
    }
    if let Some(record) = input.shape.loader_record() {
        let metadata = installed_loader_metadata_bytes(record)
            .map_err(|_| KnownGoodInventoryError::MetadataSerialization)?;
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Versions,
            path: KnownGoodRelativePath::new(&format!("{version_base}/.axial-loader.json"))?,
            kind: KnownGoodArtifactKind::LoaderMetadata,
            integrity: exact_bytes_integrity(&metadata),
        })?;
    }

    Ok(builder.finish())
}

fn version_metadata_integrity(
    shape: KnownGoodInstallShape<'_>,
    version: &VersionJson,
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
    match shape {
        KnownGoodInstallShape::Vanilla { version_manifest } => {
            if version_manifest.id != version.id {
                return Err(KnownGoodInventoryError::VersionIdentityMismatch);
            }
            expected_integrity(
                &ExpectedIntegrity::from_sha1(&version_manifest.sha1),
                false,
                "version metadata",
            )
        }
        loader_shape => {
            let record = loader_shape
                .loader_record()
                .ok_or(KnownGoodInventoryError::LoaderIdentityMismatch)?;
            if record.version_id != version.id
                || loader_shape.component() != Some(record.component_id)
                || !loader_strategy_matches(record.component_id, record.strategy)
            {
                return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
            }
            let bytes = serde_json::to_vec_pretty(version)
                .map_err(|_| KnownGoodInventoryError::MetadataSerialization)?;
            Ok(exact_bytes_integrity(&bytes))
        }
    }
}

fn loader_strategy_matches(component: LoaderComponentId, strategy: LoaderInstallStrategy) -> bool {
    matches!(
        (component, strategy),
        (
            LoaderComponentId::Fabric,
            LoaderInstallStrategy::FabricProfile
        ) | (
            LoaderComponentId::Quilt,
            LoaderInstallStrategy::QuiltProfile
        ) | (
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern
                | LoaderInstallStrategy::ForgeLegacyInstaller
                | LoaderInstallStrategy::ForgeEarliestLegacy
        ) | (
            LoaderComponentId::NeoForge,
            LoaderInstallStrategy::NeoForgeModern
        )
    )
}

fn add_library_plans(
    builder: &mut InventoryBuilder,
    plans: Vec<LibraryArtifactPlan>,
) -> Result<(), KnownGoodInventoryError> {
    for plan in plans {
        let path = KnownGoodRelativePath::new(plan.relative_path.as_str())?;
        let integrity =
            expected_integrity(&plan.expected, plan.allow_missing_checksum, path.as_str())?;
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Libraries,
            path,
            kind: if plan.is_native {
                KnownGoodArtifactKind::NativeLibrary
            } else {
                KnownGoodArtifactKind::Library
            },
            integrity,
        })?;
    }
    Ok(())
}

fn add_asset_index(
    builder: &mut InventoryBuilder,
    version: &VersionJson,
    bytes: Option<&[u8]>,
) -> Result<(), KnownGoodInventoryError> {
    if bytes.is_some_and(|bytes| bytes.len() > MAX_KNOWN_GOOD_ASSET_INDEX_BYTES) {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    if version.asset_index.id.trim().is_empty() {
        return if bytes.is_none() {
            Ok(())
        } else {
            Err(KnownGoodInventoryError::UnexpectedAssetIndex)
        };
    }
    let bytes = bytes.ok_or(KnownGoodInventoryError::MissingAssetIndex)?;
    let index_id = KnownGoodId::new(&version.asset_index.id)?;
    let expected =
        ExpectedIntegrity::from_mojang(version.asset_index.size, &version.asset_index.sha1);
    validate_bytes(bytes, &expected).map_err(|_| KnownGoodInventoryError::AssetIndexIntegrity)?;
    builder.insert(KnownGoodEntry {
        root: KnownGoodRoot::Assets,
        path: KnownGoodRelativePath::new(&format!("indexes/{}.json", index_id.as_str()))?,
        kind: KnownGoodArtifactKind::AssetIndex,
        integrity: expected_integrity(&expected, false, index_id.as_str())?,
    })?;

    let index = parse_asset_index(bytes).map_err(|_| KnownGoodInventoryError::AssetIndexParse)?;
    for object in index.objects.values() {
        let digest = Sha1Digest::from_metadata(&object.hash)
            .map_err(|_| KnownGoodInventoryError::InvalidAssetObject)?;
        let size = u64::try_from(object.size).ok().filter(|size| *size > 0);
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Assets,
            path: KnownGoodRelativePath::new(&format!(
                "objects/{}/{}",
                &digest.as_str()[..2],
                digest.as_str()
            ))?,
            kind: KnownGoodArtifactKind::AssetObject,
            integrity: KnownGoodIntegrity::Sha1 { digest, size },
        })?;
    }
    Ok(())
}

fn add_runtime(
    builder: &mut InventoryBuilder,
    input: RuntimeInventoryInput<'_>,
) -> Result<(), KnownGoodInventoryError> {
    if input.manifest_bytes.len() > MAX_KNOWN_GOOD_RUNTIME_MANIFEST_BYTES {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    validate_bytes(input.manifest_bytes, input.manifest_expected)
        .map_err(|_| KnownGoodInventoryError::RuntimeManifestIntegrity)?;
    let manifest = serde_json::from_slice::<ComponentManifest>(input.manifest_bytes)
        .map_err(|_| KnownGoodInventoryError::RuntimeManifestParse)?;
    let manifest_proof = component_manifest_proof_bytes(&manifest)
        .map_err(|_| KnownGoodInventoryError::MetadataSerialization)?;
    let root = KnownGoodRoot::ManagedRuntime {
        component: KnownGoodId::new(input.component.as_str())?,
    };
    builder.insert(KnownGoodEntry {
        root: root.clone(),
        path: KnownGoodRelativePath::new(COMPONENT_MANIFEST_PROOF_FILE)?,
        kind: KnownGoodArtifactKind::RuntimeManifestProof,
        integrity: exact_bytes_integrity(&manifest_proof),
    })?;
    builder.insert(KnownGoodEntry {
        root: root.clone(),
        path: KnownGoodRelativePath::new(".axial-ready")?,
        kind: KnownGoodArtifactKind::RuntimeReadyMarker,
        integrity: exact_bytes_integrity(b"ready"),
    })?;

    let plan = plan_runtime_manifest_files(manifest.files);
    if plan.file_entries.is_empty() {
        return Err(KnownGoodInventoryError::MissingRuntimeDownload);
    }
    for (path, _) in plan.directory_entries {
        builder.insert(KnownGoodEntry {
            root: root.clone(),
            path: KnownGoodRelativePath::new(&path)?,
            kind: KnownGoodArtifactKind::RuntimeDirectory,
            integrity: KnownGoodIntegrity::Directory,
        })?;
    }
    for (path, file) in plan.file_entries {
        let raw = file
            .downloads
            .and_then(|downloads| downloads.raw)
            .ok_or(KnownGoodInventoryError::MissingRuntimeDownload)?;
        let expected = ExpectedIntegrity {
            size: raw.size,
            sha1: raw.sha1,
        };
        builder.insert(KnownGoodEntry {
            root: root.clone(),
            path: KnownGoodRelativePath::new(&path)?,
            kind: if file.executable {
                KnownGoodArtifactKind::RuntimeExecutable
            } else {
                KnownGoodArtifactKind::RuntimeFile
            },
            integrity: expected_integrity(&expected, false, &path)?,
        })?;
    }
    for (path, file) in plan.link_entries {
        let target = file
            .target
            .ok_or(KnownGoodInventoryError::UnsupportedRuntimeEntry)?;
        builder.insert(KnownGoodEntry {
            root: root.clone(),
            path: KnownGoodRelativePath::new(&path)?,
            kind: KnownGoodArtifactKind::RuntimeLink,
            integrity: KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget::new(&path, &target)?),
        })?;
    }
    if !plan.other_entries.is_empty() {
        return Err(KnownGoodInventoryError::UnsupportedRuntimeEntry);
    }
    Ok(())
}

fn expected_integrity(
    expected: &ExpectedIntegrity,
    allow_missing_checksum: bool,
    path: &str,
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
    match expected.sha1.as_deref() {
        Some(value) => Ok(KnownGoodIntegrity::Sha1 {
            digest: Sha1Digest::from_metadata(value)?,
            size: expected.size,
        }),
        None if allow_missing_checksum && path.to_ascii_lowercase().ends_with(".jar") => {
            Ok(KnownGoodIntegrity::StructuralJar {
                size: expected.size,
            })
        }
        None => Err(KnownGoodInventoryError::MissingChecksum),
    }
}

fn exact_bytes_integrity(bytes: &[u8]) -> KnownGoodIntegrity {
    KnownGoodIntegrity::ExactBytes {
        digest: sha1_digest(bytes),
        size: bytes.len() as u64,
    }
}

fn validate_bytes(
    bytes: &[u8],
    expected: &ExpectedIntegrity,
) -> Result<(), KnownGoodInventoryError> {
    if expected.size.is_some_and(|size| size != bytes.len() as u64) {
        return Err(KnownGoodInventoryError::AssetIndexIntegrity);
    }
    let expected_digest = expected
        .sha1
        .as_deref()
        .ok_or(KnownGoodInventoryError::InvalidSha1)
        .and_then(Sha1Digest::from_metadata)?;
    if expected_digest != sha1_digest(bytes) {
        return Err(KnownGoodInventoryError::AssetIndexIntegrity);
    }
    Ok(())
}

fn sha1_digest(bytes: &[u8]) -> Sha1Digest {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    Sha1Digest(format!("{:x}", hasher.finalize()))
}

impl KnownGoodLinkTarget {
    fn new(link_path: &str, target: &str) -> Result<Self, KnownGoodInventoryError> {
        if target.trim().is_empty()
            || target.len() > MAX_KNOWN_GOOD_RELATIVE_PATH_BYTES
            || target.starts_with('/')
            || target.starts_with('\\')
            || windows_prefixed(target)
            || target.chars().any(char::is_control)
        {
            return Err(KnownGoodInventoryError::UnsafePath);
        }
        let link = KnownGoodRelativePath::new(link_path)?;
        let mut resolved = link
            .as_str()
            .split('/')
            .map(str::to_string)
            .collect::<Vec<_>>();
        resolved.pop();
        let mut normalized_target: Vec<String> = Vec::new();
        for segment in target.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => {
                    if resolved.pop().is_none() {
                        return Err(KnownGoodInventoryError::UnsafePath);
                    }
                    if normalized_target
                        .last()
                        .is_some_and(|segment| segment != "..")
                    {
                        normalized_target.pop();
                    } else {
                        normalized_target.push("..".to_string());
                    }
                }
                value if unsafe_link_target_segment(value) => {
                    return Err(KnownGoodInventoryError::UnsafePath);
                }
                value => {
                    resolved.push(value.to_string());
                    normalized_target.push(value.to_string());
                }
            }
        }
        if normalized_target.is_empty() {
            return Err(KnownGoodInventoryError::UnsafePath);
        }
        Ok(Self(normalized_target.join("/")))
    }
}

fn unsafe_link_target_segment(segment: &str) -> bool {
    segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.len() > MAX_KNOWN_GOOD_PATH_SEGMENT_BYTES
        || segment.contains(':')
        || segment.chars().any(char::is_control)
        || Path::new(segment)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
}

fn windows_prefixed(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || value.starts_with("//")
        || value.starts_with("\\\\")
        || value.starts_with("\\?\\")
        || value.starts_with("\\.\\")
}

#[derive(Default)]
struct InventoryBuilder {
    entries: BTreeMap<(String, String, String), KnownGoodEntry>,
}

impl InventoryBuilder {
    fn insert(&mut self, entry: KnownGoodEntry) -> Result<(), KnownGoodInventoryError> {
        let key = (
            entry.root.stable_id().to_string(),
            entry.root.scope_id().to_string(),
            entry.path.as_str().to_string(),
        );
        if let Some(existing) = self.entries.get(&key) {
            if existing == &entry {
                return Ok(());
            }
            return Err(KnownGoodInventoryError::ConflictingEntry);
        }
        if self.entries.len() >= MAX_KNOWN_GOOD_ENTRIES {
            return Err(KnownGoodInventoryError::TooManyEntries);
        }
        self.entries.insert(key, entry);
        Ok(())
    }

    fn finish(self) -> KnownGoodInventory {
        KnownGoodInventory {
            entries: self.entries.into_values().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{
        ArgumentsSection, AssetIndex, Downloads, JavaVersion, LibraryArtifact, LibraryDownload,
        LoggingConf, LoggingEntry, LoggingFile,
    };
    use crate::loaders::types::LoaderBuildSubjectKind;
    use crate::loaders::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderInstallSource, LoaderInstallability,
    };
    use std::collections::HashMap;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn vanilla_fixture_derives_producer_declared_inventory() {
        let fixture = fixture(FixtureShape::Vanilla, false);
        let inventory = derive_known_good_inventory(fixture.input()).expect("vanilla inventory");

        assert_entry(
            &inventory,
            &KnownGoodRoot::Versions,
            "fixture-version/fixture-version.json",
            KnownGoodArtifactKind::VersionMetadata,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_C).unwrap(),
                size: None,
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Versions,
            "fixture-version/fixture-version.jar",
            KnownGoodArtifactKind::ClientJar,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).unwrap(),
                size: Some(40),
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "com/mojang/strict/1.0/strict-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).unwrap(),
                size: Some(10),
            },
        );
        let asset_index = entry(
            &inventory,
            &KnownGoodRoot::Assets,
            "indexes/fixture-assets.json",
        );
        assert_eq!(asset_index.kind(), KnownGoodArtifactKind::AssetIndex);
        assert_eq!(
            asset_index.integrity(),
            &KnownGoodIntegrity::Sha1 {
                digest: sha1_digest(&fixture.asset_index),
                size: Some(fixture.asset_index.len() as u64),
            }
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Assets,
            &format!("objects/aa/{SHA_A}"),
            KnownGoodArtifactKind::AssetObject,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).unwrap(),
                size: Some(5),
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Assets,
            "log_configs/client-log.xml",
            KnownGoodArtifactKind::LogConfig,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_C).unwrap(),
                size: Some(15),
            },
        );
        assert_runtime_entries(&inventory);
        assert!(!has_kind(&inventory, KnownGoodArtifactKind::LoaderMetadata));
        assert!(
            !inventory
                .entries()
                .iter()
                .any(|entry| matches!(entry.integrity(), KnownGoodIntegrity::StructuralJar { .. }))
        );
        assert_sorted_unique(&inventory);
    }

    #[test]
    fn fabric_fixture_keeps_checksumless_loader_jar_explicitly_unverified() {
        assert_profile_loader_fixture(FixtureShape::Fabric);
    }

    #[test]
    fn quilt_fixture_keeps_checksumless_loader_jar_explicitly_unverified() {
        assert_profile_loader_fixture(FixtureShape::Quilt);
    }

    #[test]
    fn forge_fixture_includes_installer_libraries_without_arbitrary_processor_outputs() {
        assert_installer_loader_fixture(FixtureShape::Forge);
    }

    #[test]
    fn neoforge_fixture_includes_installer_libraries_without_arbitrary_processor_outputs() {
        assert_installer_loader_fixture(FixtureShape::NeoForge);
    }

    #[test]
    fn relative_paths_reject_user_roots_traversal_drive_and_unc_on_every_host() {
        for path in [
            "../mods/example.jar",
            "versions/../mods/example.jar",
            "/absolute/client.jar",
            r"C:\Users\Alice\client.jar",
            r"\\server\share\client.jar",
            r"\\?\C:\client.jar",
            "config//options.txt",
            "saves/./world",
        ] {
            assert!(
                KnownGoodRelativePath::new(path).is_err(),
                "unsafe path accepted: {path}"
            );
        }

        let representable_roots = [
            KnownGoodRoot::Versions,
            KnownGoodRoot::Libraries,
            KnownGoodRoot::Assets,
            KnownGoodRoot::ManagedRuntime {
                component: KnownGoodId::new("java-runtime-delta").unwrap(),
            },
        ];
        assert_eq!(
            representable_roots
                .iter()
                .map(KnownGoodRoot::stable_id)
                .collect::<Vec<_>>(),
            ["versions", "libraries", "assets", "managed_runtime"]
        );
    }

    #[test]
    fn shuffled_metadata_derives_identical_sorted_inventory() {
        let mut left = fixture(FixtureShape::Vanilla, false);
        let mut right = fixture(FixtureShape::Vanilla, true);
        let extra = checksum_library(
            "com.mojang:also-strict:1.0",
            "com/mojang/also-strict/1.0/also-strict-1.0.jar",
            SHA_B,
            20,
        );
        left.version.libraries.push(extra.clone());
        right.version.libraries.push(extra);
        right.version.libraries.reverse();
        let left = derive_known_good_inventory(left.input()).unwrap();
        let right = derive_known_good_inventory(right.input()).unwrap();

        assert_eq!(left, right);
        assert_sorted_unique(&left);
    }

    #[test]
    fn duplicate_path_with_different_contract_fails_closed() {
        let mut builder = InventoryBuilder::default();
        let path = KnownGoodRelativePath::new("shared/artifact.jar").unwrap();
        builder
            .insert(KnownGoodEntry {
                root: KnownGoodRoot::Libraries,
                path: path.clone(),
                kind: KnownGoodArtifactKind::Library,
                integrity: KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata(SHA_A).unwrap(),
                    size: Some(10),
                },
            })
            .unwrap();
        let error = builder
            .insert(KnownGoodEntry {
                root: KnownGoodRoot::Libraries,
                path,
                kind: KnownGoodArtifactKind::Library,
                integrity: KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata(SHA_B).unwrap(),
                    size: Some(10),
                },
            })
            .expect_err("conflicting contract");
        assert!(matches!(error, KnownGoodInventoryError::ConflictingEntry));
    }

    #[test]
    fn runtime_inventory_uses_raw_integrity_not_lzma_transport_integrity() {
        let fixture = fixture(FixtureShape::Vanilla, false);
        let inventory = derive_known_good_inventory(fixture.input()).unwrap();
        let runtime = inventory
            .entries()
            .iter()
            .find(|entry| entry.kind() == KnownGoodArtifactKind::RuntimeExecutable)
            .expect("runtime executable");
        let KnownGoodIntegrity::Sha1 { digest, size } = runtime.integrity() else {
            panic!("runtime executable must have raw integrity")
        };
        assert_eq!(digest.as_str(), SHA_B);
        assert_eq!(*size, Some(20));
    }

    #[test]
    fn asset_objects_deduplicate_by_content_address() {
        let fixture = fixture(FixtureShape::Vanilla, false);
        let inventory = derive_known_good_inventory(fixture.input()).unwrap();
        let objects = inventory
            .entries()
            .iter()
            .filter(|entry| entry.kind() == KnownGoodArtifactKind::AssetObject)
            .collect::<Vec<_>>();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].path().as_str(), format!("objects/aa/{SHA_A}"));
    }

    #[test]
    fn vanilla_missing_library_checksum_fails_closed() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture
            .version
            .libraries
            .push(checksumless_loader_library());

        let error = derive_known_good_inventory(fixture.input()).expect_err("missing checksum");
        assert_eq!(error, KnownGoodInventoryError::MissingChecksum);
    }

    #[test]
    fn vanilla_version_identity_mismatch_fails_closed() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.version_manifest.id = "different-version".to_string();

        let error = derive_known_good_inventory(fixture.input()).expect_err("version mismatch");
        assert_eq!(error, KnownGoodInventoryError::VersionIdentityMismatch);
    }

    #[test]
    fn loader_component_identity_mismatch_fails_closed() {
        let mut fixture = fixture(FixtureShape::Fabric, false);
        fixture.loader_record.as_mut().unwrap().component_id = LoaderComponentId::Quilt;

        let error = derive_known_good_inventory(fixture.input()).expect_err("loader mismatch");
        assert_eq!(error, KnownGoodInventoryError::LoaderIdentityMismatch);
    }

    #[test]
    fn loader_version_identity_mismatch_fails_closed() {
        let mut fixture = fixture(FixtureShape::Forge, false);
        fixture.loader_record.as_mut().unwrap().version_id = "different-version".to_string();

        let error = derive_known_good_inventory(fixture.input()).expect_err("loader mismatch");
        assert_eq!(error, KnownGoodInventoryError::LoaderIdentityMismatch);
    }

    #[test]
    fn runtime_component_identity_mismatch_fails_closed() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.runtime_id = RuntimeId::from("java-runtime-gamma");

        let error = derive_known_good_inventory(fixture.input()).expect_err("runtime mismatch");
        assert_eq!(error, KnownGoodInventoryError::RuntimeIdentityMismatch);
    }

    #[test]
    fn unsafe_library_plan_maps_to_closed_inventory_error() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.version.libraries.push(checksum_library(
            "net.example:unsafe:1.0",
            "../outside.jar",
            SHA_A,
            10,
        ));

        let error = derive_known_good_inventory(fixture.input()).expect_err("unsafe plan");
        assert_eq!(error, KnownGoodInventoryError::InvalidLibraryPlan);
    }

    #[test]
    fn conflicting_library_plan_maps_to_closed_inventory_error() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.version.libraries.push(checksum_library(
            "net.example:conflict:1.0",
            "com/mojang/strict/1.0/strict-1.0.jar",
            SHA_B,
            11,
        ));

        let error = derive_known_good_inventory(fixture.input()).expect_err("conflicting plan");
        assert_eq!(error, KnownGoodInventoryError::InvalidLibraryPlan);
    }

    #[test]
    fn runtime_manifest_proof_is_canonical_across_provider_object_order() {
        let left = fixture(FixtureShape::Vanilla, false);
        let right = fixture(FixtureShape::Vanilla, true);
        let left = derive_known_good_inventory(left.input()).unwrap();
        let right = derive_known_good_inventory(right.input()).unwrap();
        let root = runtime_root();

        let left = entry(&left, &root, COMPONENT_MANIFEST_PROOF_FILE);
        let right = entry(&right, &root, COMPONENT_MANIFEST_PROOF_FILE);
        assert_eq!(left.kind(), KnownGoodArtifactKind::RuntimeManifestProof);
        assert_eq!(left.integrity(), right.integrity());
        assert!(matches!(
            left.integrity(),
            KnownGoodIntegrity::ExactBytes { .. }
        ));
    }

    #[test]
    fn runtime_link_target_is_stored_canonically() {
        let fixture = fixture(FixtureShape::Vanilla, false);
        let inventory = derive_known_good_inventory(fixture.input()).unwrap();
        let link = entry(&inventory, &runtime_root(), "java-link");

        assert_eq!(link.kind(), KnownGoodArtifactKind::RuntimeLink);
        let KnownGoodIntegrity::LinkTarget(target) = link.integrity() else {
            panic!("runtime link must carry its canonical target")
        };
        assert_eq!(target.as_str(), "bin/java");
    }

    #[test]
    fn oversized_runtime_manifest_is_rejected_before_parsing() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.runtime_manifest_bytes = vec![b' '; MAX_KNOWN_GOOD_RUNTIME_MANIFEST_BYTES + 1];
        fixture.runtime_manifest_expected = expected_for(&fixture.runtime_manifest_bytes);

        let error = derive_known_good_inventory(fixture.input()).expect_err("oversized manifest");
        assert_eq!(error, KnownGoodInventoryError::InputTooLarge);
    }

    fn assert_profile_loader_fixture(shape: FixtureShape) {
        let fixture = fixture(shape, false);
        let component = fixture.loader_record.as_ref().unwrap().component_id;
        let inventory = derive_known_good_inventory(fixture.input()).expect("profile inventory");
        assert_loader_metadata(&fixture, &inventory, component);
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "net/loader/loader-unverified/1.0/loader-unverified-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::StructuralJar { size: Some(12) },
        );
        assert_eq!(structural_jar_count(&inventory), 1);
        assert_sorted_unique(&inventory);
    }

    fn assert_installer_loader_fixture(shape: FixtureShape) {
        let fixture = fixture(shape, false);
        let component = fixture.loader_record.as_ref().unwrap().component_id;
        let inventory = derive_known_good_inventory(fixture.input()).expect("installer inventory");
        assert_loader_metadata(&fixture, &inventory, component);
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "net/example/installer-only/1.0/installer-only-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_C).unwrap(),
                size: Some(30),
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "net/loader/loader-unverified/1.0/loader-unverified-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::StructuralJar { size: Some(12) },
        );
        assert_eq!(structural_jar_count(&inventory), 1);
        assert!(
            !inventory
                .entries()
                .iter()
                .any(|entry| entry.path().as_str().contains("processor-output"))
        );
        assert_sorted_unique(&inventory);
    }

    fn assert_loader_metadata(
        fixture: &Fixture,
        inventory: &KnownGoodInventory,
        component: LoaderComponentId,
    ) {
        let record = fixture.loader_record.as_ref().unwrap();
        assert_eq!(record.component_id, component);
        let expected = installed_loader_metadata_bytes(record).unwrap();
        assert_entry(
            inventory,
            &KnownGoodRoot::Versions,
            "fixture-version/.axial-loader.json",
            KnownGoodArtifactKind::LoaderMetadata,
            &exact_bytes_integrity(&expected),
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FixtureShape {
        Vanilla,
        Fabric,
        Quilt,
        Forge,
        NeoForge,
    }

    impl FixtureShape {
        fn component(self) -> Option<LoaderComponentId> {
            match self {
                Self::Vanilla => None,
                Self::Fabric => Some(LoaderComponentId::Fabric),
                Self::Quilt => Some(LoaderComponentId::Quilt),
                Self::Forge => Some(LoaderComponentId::Forge),
                Self::NeoForge => Some(LoaderComponentId::NeoForge),
            }
        }

        fn strategy(self) -> Option<LoaderInstallStrategy> {
            match self {
                Self::Vanilla => None,
                Self::Fabric => Some(LoaderInstallStrategy::FabricProfile),
                Self::Quilt => Some(LoaderInstallStrategy::QuiltProfile),
                Self::Forge => Some(LoaderInstallStrategy::ForgeModern),
                Self::NeoForge => Some(LoaderInstallStrategy::NeoForgeModern),
            }
        }
    }

    struct Fixture {
        version: VersionJson,
        version_manifest: ManifestEntry,
        asset_index: Vec<u8>,
        runtime_manifest_bytes: Vec<u8>,
        runtime_manifest_expected: ExpectedIntegrity,
        runtime_id: RuntimeId,
        shape: FixtureShape,
        loader_record: Option<LoaderBuildRecord>,
        installer_libraries: Vec<Library>,
        environment: Environment,
    }

    impl Fixture {
        fn input(&self) -> KnownGoodInventoryInput<'_> {
            let record = self.loader_record.as_ref();
            let shape = match self.shape {
                FixtureShape::Vanilla => KnownGoodInstallShape::Vanilla {
                    version_manifest: &self.version_manifest,
                },
                FixtureShape::Fabric => KnownGoodInstallShape::Fabric {
                    record: record.expect("fabric record"),
                },
                FixtureShape::Quilt => KnownGoodInstallShape::Quilt {
                    record: record.expect("quilt record"),
                },
                FixtureShape::Forge => KnownGoodInstallShape::Forge {
                    record: record.expect("forge record"),
                    installer_libraries: &self.installer_libraries,
                },
                FixtureShape::NeoForge => KnownGoodInstallShape::NeoForge {
                    record: record.expect("neoforge record"),
                    installer_libraries: &self.installer_libraries,
                },
            };
            KnownGoodInventoryInput {
                resolved_version: &self.version,
                asset_index_bytes: Some(&self.asset_index),
                runtime: Some(RuntimeInventoryInput {
                    component: &self.runtime_id,
                    manifest_bytes: &self.runtime_manifest_bytes,
                    manifest_expected: &self.runtime_manifest_expected,
                }),
                shape,
                environment: &self.environment,
            }
        }
    }

    fn fixture(shape: FixtureShape, shuffled: bool) -> Fixture {
        let asset_index = format!(
            r#"{{"objects":{{"first":{{"hash":"{SHA_A}","size":5}},"second":{{"hash":"{SHA_A}","size":5}}}}}}"#
        )
        .into_bytes();
        let asset_sha1 = sha1_digest(&asset_index).as_str().to_string();
        let mut libraries = vec![checksum_library(
            "com.mojang:strict:1.0",
            "com/mojang/strict/1.0/strict-1.0.jar",
            SHA_A,
            10,
        )];
        if shape.component().is_some() {
            libraries.push(checksumless_loader_library());
        }
        if shuffled {
            libraries.reverse();
        }
        let version = VersionJson {
            id: "fixture-version".to_string(),
            inherits_from: String::new(),
            kind: "release".to_string(),
            main_class: "net.minecraft.client.main.Main".to_string(),
            minimum_launcher_version: 0,
            compliance_level: 1,
            release_time: String::new(),
            time: String::new(),
            arguments: Some(ArgumentsSection::default()),
            minecraft_arguments: String::new(),
            asset_index: AssetIndex {
                id: "fixture-assets".to_string(),
                sha1: asset_sha1,
                size: asset_index.len() as i64,
                total_size: 5,
                url: "https://example.invalid/assets".to_string(),
            },
            assets: "fixture-assets".to_string(),
            downloads: Downloads {
                client: Some(crate::launch::DownloadEntry {
                    sha1: SHA_A.to_string(),
                    size: 40,
                    url: "https://example.invalid/client".to_string(),
                }),
                ..Downloads::default()
            },
            java_version: JavaVersion {
                component: "java-runtime-delta".to_string(),
                major_version: 21,
            },
            libraries,
            logging: Some(LoggingConf {
                client: Some(LoggingEntry {
                    argument: "-Dlog4j.configurationFile=${path}".to_string(),
                    file: LoggingFile {
                        id: "client-log.xml".to_string(),
                        sha1: SHA_C.to_string(),
                        size: 15,
                        url: "https://example.invalid/log-config".to_string(),
                    },
                    kind: "log4j2-xml".to_string(),
                }),
            }),
        };
        let runtime_manifest_bytes = runtime_manifest_bytes(shuffled);
        let runtime_manifest_expected = expected_for(&runtime_manifest_bytes);
        let loader_record = shape
            .component()
            .map(|component| loader_record(shape, component));
        let installer_libraries = if matches!(shape, FixtureShape::Forge | FixtureShape::NeoForge) {
            vec![checksum_library(
                "net.example:installer-only:1.0",
                "net/example/installer-only/1.0/installer-only-1.0.jar",
                SHA_C,
                30,
            )]
        } else {
            Vec::new()
        };
        Fixture {
            version,
            version_manifest: ManifestEntry {
                id: "fixture-version".to_string(),
                kind: "release".to_string(),
                url: "https://example.invalid/version".to_string(),
                time: String::new(),
                release_time: String::new(),
                sha1: SHA_C.to_string(),
                compliance_level: 1,
            },
            asset_index,
            runtime_manifest_bytes,
            runtime_manifest_expected,
            runtime_id: RuntimeId::from("java-runtime-delta"),
            shape,
            loader_record,
            installer_libraries,
            environment: Environment {
                os_name: "linux".to_string(),
                os_arch: "x86_64".to_string(),
                os_version: String::new(),
                features: HashMap::new(),
            },
        }
    }

    fn loader_record(shape: FixtureShape, component: LoaderComponentId) -> LoaderBuildRecord {
        let strategy = shape.strategy().expect("loader strategy");
        let (artifact_kind, install_source) = match shape {
            FixtureShape::Fabric | FixtureShape::Quilt => (
                LoaderArtifactKind::ProfileJson,
                LoaderInstallSource::ProfileJson {
                    url: "https://example.invalid/profile".to_string(),
                },
            ),
            FixtureShape::Forge | FixtureShape::NeoForge => (
                LoaderArtifactKind::InstallerJar,
                LoaderInstallSource::InstallerJar {
                    url: "https://example.invalid/installer".to_string(),
                },
            ),
            FixtureShape::Vanilla => unreachable!(),
        };
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id: component,
            component_name: component.display_name().to_string(),
            build_id: format!("{}-fixture-build", component.short_key()),
            minecraft_version: "1.21.1".to_string(),
            loader_version: "1.0".to_string(),
            version_id: "fixture-version".to_string(),
            build_meta: LoaderBuildMetadata::default(),
            strategy,
            artifact_kind,
            installability: LoaderInstallability::Installable,
            install_source,
        }
    }

    fn checksum_library(name: &str, path: &str, sha1: &str, size: i64) -> Library {
        Library {
            name: name.to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: path.to_string(),
                    sha1: sha1.to_string(),
                    size,
                    url: "https://example.invalid/library".to_string(),
                }),
                classifiers: HashMap::new(),
            }),
            ..Library::default()
        }
    }

    fn checksumless_loader_library() -> Library {
        Library {
            name: "net.loader:loader-unverified:1.0".to_string(),
            url: "https://example.invalid/maven/".to_string(),
            size: 12,
            ..Library::default()
        }
    }

    fn runtime_manifest_bytes(shuffled: bool) -> Vec<u8> {
        let executable = format!(
            r#""bin/java":{{"type":"file","executable":true,"downloads":{{"raw":{{"url":"https://example.invalid/java","sha1":"{SHA_B}","size":20}},"lzma":{{"url":"https://example.invalid/java.lzma","sha1":"{SHA_C}","size":10}}}}}}"#
        );
        let directory = r#""bin":{"type":"directory"}"#;
        let link = r#""java-link":{"type":"link","target":"./bin/../bin/java"}"#;
        let files = if shuffled {
            format!("{link},{executable},{directory}")
        } else {
            format!("{directory},{executable},{link}")
        };
        format!(r#"{{"files":{{{files}}}}}"#).into_bytes()
    }

    fn expected_for(bytes: &[u8]) -> ExpectedIntegrity {
        ExpectedIntegrity {
            size: Some(bytes.len() as u64),
            sha1: Some(sha1_digest(bytes).as_str().to_string()),
        }
    }

    fn runtime_root() -> KnownGoodRoot {
        KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").unwrap(),
        }
    }

    fn assert_runtime_entries(inventory: &KnownGoodInventory) {
        let root = runtime_root();
        let proof = entry(inventory, &root, COMPONENT_MANIFEST_PROOF_FILE);
        assert_eq!(proof.kind(), KnownGoodArtifactKind::RuntimeManifestProof);
        assert!(matches!(
            proof.integrity(),
            KnownGoodIntegrity::ExactBytes { .. }
        ));
        assert_entry(
            inventory,
            &root,
            ".axial-ready",
            KnownGoodArtifactKind::RuntimeReadyMarker,
            &exact_bytes_integrity(b"ready"),
        );
        assert_entry(
            inventory,
            &root,
            "bin",
            KnownGoodArtifactKind::RuntimeDirectory,
            &KnownGoodIntegrity::Directory,
        );
        assert_entry(
            inventory,
            &root,
            "bin/java",
            KnownGoodArtifactKind::RuntimeExecutable,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_B).unwrap(),
                size: Some(20),
            },
        );
        assert_entry(
            inventory,
            &root,
            "java-link",
            KnownGoodArtifactKind::RuntimeLink,
            &KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget("bin/java".to_string())),
        );
    }

    fn has_kind(inventory: &KnownGoodInventory, kind: KnownGoodArtifactKind) -> bool {
        inventory.entries().iter().any(|entry| entry.kind() == kind)
    }

    fn structural_jar_count(inventory: &KnownGoodInventory) -> usize {
        inventory
            .entries()
            .iter()
            .filter(|entry| matches!(entry.integrity(), KnownGoodIntegrity::StructuralJar { .. }))
            .count()
    }

    fn entry<'a>(
        inventory: &'a KnownGoodInventory,
        root: &KnownGoodRoot,
        path: &str,
    ) -> &'a KnownGoodEntry {
        inventory
            .entries()
            .iter()
            .find(|entry| entry.root() == root && entry.path().as_str() == path)
            .unwrap_or_else(|| panic!("missing inventory entry {root:?}:{path}"))
    }

    fn assert_entry(
        inventory: &KnownGoodInventory,
        root: &KnownGoodRoot,
        path: &str,
        kind: KnownGoodArtifactKind,
        integrity: &KnownGoodIntegrity,
    ) {
        let entry = entry(inventory, root, path);
        assert_eq!(entry.kind(), kind, "unexpected kind for {path}");
        assert_eq!(
            entry.integrity(),
            integrity,
            "unexpected integrity for {path}"
        );
    }

    fn assert_sorted_unique(inventory: &KnownGoodInventory) {
        let keys = inventory
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.root().stable_id(),
                    entry.root().scope_id(),
                    entry.path().as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
