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
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const MAX_KNOWN_GOOD_RELATIVE_PATH_BYTES: usize = MAX_ARTIFACT_RELATIVE_PATH_BYTES;
pub const MAX_KNOWN_GOOD_PATH_SEGMENT_BYTES: usize = MAX_ARTIFACT_PATH_SEGMENT_BYTES;
pub const MAX_KNOWN_GOOD_ENTRIES: usize = 200_000;
pub const MAX_KNOWN_GOOD_VERSION_JSON_BYTES: usize = 16 << 20;
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

#[derive(Clone, Debug)]
pub struct KnownGoodInventoryAuthority {
    inventory: Arc<KnownGoodInventory>,
    library_root: PathBuf,
}

impl KnownGoodInventoryAuthority {
    pub fn bind(inventory: Arc<KnownGoodInventory>, library_root: &Path) -> io::Result<Self> {
        let library_root = std::fs::canonicalize(library_root)?;
        let metadata = std::fs::symlink_metadata(&library_root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "known-good library authority requires an existing directory",
            ));
        }
        Ok(Self {
            inventory,
            library_root,
        })
    }

    pub(crate) fn authorizes_structural_library(
        &self,
        requested_library_root: &Path,
        path: &ArtifactRelativePath,
        is_native: bool,
        size: Option<u64>,
    ) -> Option<PathBuf> {
        let requested_library_root = std::fs::canonicalize(requested_library_root).ok()?;
        (requested_library_root == self.library_root
            && self
                .inventory
                .authorizes_structural_library(path, is_native, size))
        .then(|| self.library_root.clone())
    }
}

impl KnownGoodInventory {
    pub fn entries(&self) -> &[KnownGoodEntry] {
        &self.entries
    }

    pub(crate) fn authorizes_structural_library(
        &self,
        path: &ArtifactRelativePath,
        is_native: bool,
        size: Option<u64>,
    ) -> bool {
        let key = ("libraries", "", path.as_str());
        let Ok(index) = self.entries.binary_search_by(|entry| {
            (
                entry.root.stable_id(),
                entry.root.scope_id(),
                entry.path.as_str(),
            )
                .cmp(&key)
        }) else {
            return false;
        };
        let entry = &self.entries[index];
        entry.root == KnownGoodRoot::Libraries
            && entry.kind
                == if is_native {
                    KnownGoodArtifactKind::NativeLibrary
                } else {
                    KnownGoodArtifactKind::Library
                }
            && entry.integrity == (KnownGoodIntegrity::StructuralJar { size })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct KnownGoodInstallReceipt {
    version_id: KnownGoodId,
    inventory: KnownGoodInventory,
    effective_version: VersionJson,
    environment: Environment,
}

impl KnownGoodInstallReceipt {
    pub fn version_id(&self) -> &str {
        self.version_id.as_str()
    }

    pub fn into_inventory(self) -> KnownGoodInventory {
        self.inventory
    }

    pub(crate) fn effective_version(&self) -> &VersionJson {
        &self.effective_version
    }

    #[cfg(test)]
    pub(crate) fn from_test_authenticated_version(
        effective_version: VersionJson,
        environment: Environment,
    ) -> Self {
        Self {
            version_id: KnownGoodId::new(&effective_version.id).expect("safe test version id"),
            inventory: InventoryBuilder::default().finish(),
            effective_version,
            environment,
        }
    }

    pub(crate) fn authenticated_client_integrity(
        &self,
    ) -> Result<ExpectedIntegrity, KnownGoodInventoryError> {
        let client = self
            .effective_version
            .downloads
            .client
            .as_ref()
            .ok_or(KnownGoodInventoryError::MissingClient)?;
        let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
        expected_integrity(&expected, false, "authenticated client")?;
        Ok(expected)
    }

    pub(crate) fn authenticate_client_bytes(
        &self,
        bytes: &[u8],
    ) -> Result<(), KnownGoodInventoryError> {
        validate_bytes(bytes, &self.authenticated_client_integrity()?)
            .map_err(|_| KnownGoodInventoryError::ClientIntegrity)
    }

    pub(crate) fn from_verified_legacy_archive_source(
        base: &Self,
        record: &LoaderBuildRecord,
        resolved_version: VersionJson,
        version_bytes: &[u8],
        child_client_bytes: &[u8],
        loader_metadata_bytes: &[u8],
    ) -> Result<Self, KnownGoodInventoryError> {
        if base.version_id.as_str() != record.minecraft_version
            || base.effective_version.id != record.minecraft_version
            || record.component_id != LoaderComponentId::Forge
            || record.strategy != LoaderInstallStrategy::ForgeEarliestLegacy
            || crate::loaders::api::validate_loader_build_record_identity(record).is_err()
        {
            return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
        }

        let child_size = i64::try_from(child_client_bytes.len())
            .map_err(|_| KnownGoodInventoryError::InputTooLarge)?;
        let child_digest = sha1_digest(child_client_bytes);
        let child_sha1 = child_digest.as_str().to_string();
        let mut expected_version = base.effective_version.clone();
        expected_version.id = record.version_id.clone();
        expected_version.inherits_from = record.minecraft_version.clone();
        expected_version.materialized = true;
        let client = expected_version
            .downloads
            .client
            .as_mut()
            .ok_or(KnownGoodInventoryError::MissingClient)?;
        client.sha1 = child_sha1;
        client.size = child_size;
        client.url.clear();
        if resolved_version != expected_version
            || !serde_json::from_slice::<VersionJson>(version_bytes)
                .is_ok_and(|version| version == resolved_version)
        {
            return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
        }

        let version_id = KnownGoodId::new(&resolved_version.id)?;
        let mut builder = InventoryBuilder::default();
        for entry in base.inventory.entries.iter().filter(|entry| {
            matches!(
                &entry.root,
                KnownGoodRoot::Assets | KnownGoodRoot::ManagedRuntime { .. }
            )
        }) {
            builder.insert(entry.clone())?;
        }
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Versions,
            path: KnownGoodRelativePath::new(&format!("{0}/{0}.json", version_id.as_str()))?,
            kind: KnownGoodArtifactKind::VersionMetadata,
            integrity: exact_bytes_integrity(version_bytes),
        })?;
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Versions,
            path: KnownGoodRelativePath::new(&format!("{0}/{0}.jar", version_id.as_str()))?,
            kind: KnownGoodArtifactKind::ClientJar,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: child_digest,
                size: Some(child_client_bytes.len() as u64),
            },
        })?;

        let libraries = library_artifact_plans_for(
            &resolved_version.libraries,
            &base.environment,
            LibraryChecksumPolicy::Strict,
        )
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
        add_library_plans(&mut builder, libraries, &BTreeMap::new())?;

        let expected_metadata = installed_loader_metadata_bytes(record)
            .map_err(|_| KnownGoodInventoryError::MetadataSerialization)?;
        if expected_metadata != loader_metadata_bytes {
            return Err(KnownGoodInventoryError::MetadataSerialization);
        }
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Versions,
            path: KnownGoodRelativePath::new(&format!(
                "{}/.axial-loader.json",
                version_id.as_str()
            ))?,
            kind: KnownGoodArtifactKind::LoaderMetadata,
            integrity: exact_bytes_integrity(loader_metadata_bytes),
        })?;

        Ok(Self {
            version_id,
            inventory: builder.finish(),
            effective_version: resolved_version,
            environment: base.environment.clone(),
        })
    }

    pub(crate) fn from_verified_profile_source(
        base: &Self,
        record: &LoaderBuildRecord,
        resolved_version: VersionJson,
        version_bytes: &[u8],
        profile_libraries: &[Library],
        loader_metadata_bytes: &[u8],
    ) -> Result<Self, KnownGoodInventoryError> {
        if base.version_id.as_str() != record.minecraft_version
            || base.effective_version.id != record.minecraft_version
            || resolved_version.id != record.version_id
            || resolved_version.inherits_from != record.minecraft_version
            || !resolved_version.materialized
            || resolved_version.asset_index != base.effective_version.asset_index
            || resolved_version.assets != base.effective_version.assets
            || resolved_version.downloads != base.effective_version.downloads
            || resolved_version.java_version != base.effective_version.java_version
            || resolved_version.logging != base.effective_version.logging
            || !serde_json::from_slice::<VersionJson>(version_bytes)
                .is_ok_and(|version| version == resolved_version)
        {
            return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
        }
        let component_matches = matches!(
            (record.component_id, record.strategy),
            (
                LoaderComponentId::Fabric,
                LoaderInstallStrategy::FabricProfile
            ) | (
                LoaderComponentId::Quilt,
                LoaderInstallStrategy::QuiltProfile
            )
        );
        if !component_matches
            || crate::loaders::api::validate_loader_build_record_identity(record).is_err()
        {
            return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
        }

        let version_id = KnownGoodId::new(&resolved_version.id)?;
        let mut builder = InventoryBuilder::default();
        for entry in base.inventory.entries.iter().filter(|entry| {
            matches!(
                &entry.root,
                KnownGoodRoot::Assets | KnownGoodRoot::ManagedRuntime { .. }
            )
        }) {
            builder.insert(entry.clone())?;
        }

        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Versions,
            path: KnownGoodRelativePath::new(&format!("{0}/{0}.json", version_id.as_str()))?,
            kind: KnownGoodArtifactKind::VersionMetadata,
            integrity: exact_bytes_integrity(version_bytes),
        })?;
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Versions,
            path: KnownGoodRelativePath::new(&format!("{0}/{0}.jar", version_id.as_str()))?,
            kind: KnownGoodArtifactKind::ClientJar,
            integrity: expected_integrity(
                &base.authenticated_client_integrity()?,
                false,
                "authenticated client",
            )?,
        })?;

        if resolved_version
            .libraries
            .len()
            .saturating_add(profile_libraries.len())
            > MAX_KNOWN_GOOD_ENTRIES
        {
            return Err(KnownGoodInventoryError::InputTooLarge);
        }
        let structural_proofs = structural_library_proofs(
            library_artifact_plans_for(
                profile_libraries,
                &base.environment,
                LibraryChecksumPolicy::AllowMissing,
            )
            .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?,
        )?;
        let libraries = library_artifact_plans_for(
            &resolved_version.libraries,
            &base.environment,
            LibraryChecksumPolicy::Strict,
        )
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
        add_library_plans(&mut builder, libraries, &structural_proofs)?;

        let expected_metadata = installed_loader_metadata_bytes(record)
            .map_err(|_| KnownGoodInventoryError::MetadataSerialization)?;
        if expected_metadata != loader_metadata_bytes {
            return Err(KnownGoodInventoryError::MetadataSerialization);
        }
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Versions,
            path: KnownGoodRelativePath::new(&format!(
                "{}/.axial-loader.json",
                version_id.as_str()
            ))?,
            kind: KnownGoodArtifactKind::LoaderMetadata,
            integrity: exact_bytes_integrity(loader_metadata_bytes),
        })?;

        Ok(Self {
            version_id,
            inventory: builder.finish(),
            effective_version: resolved_version,
            environment: base.environment.clone(),
        })
    }

    pub(crate) fn from_verified_vanilla_source(
        input: KnownGoodInventoryInput<'_>,
        version_json_bytes: &[u8],
    ) -> Result<Self, KnownGoodInventoryError> {
        let KnownGoodInstallShape::Vanilla { version_manifest } = input.shape else {
            return Err(KnownGoodInventoryError::VersionIdentityMismatch);
        };
        validate_bytes(
            version_json_bytes,
            &ExpectedIntegrity::from_sha1(&version_manifest.sha1),
        )
        .map_err(|_| KnownGoodInventoryError::VersionMetadataIntegrity)?;
        let version_id = KnownGoodId::new(&input.resolved_version.id)?;
        let effective_version = input.resolved_version.clone();
        let environment = input.environment.clone();
        let inventory = derive_known_good_inventory(input)?;
        Ok(Self {
            version_id,
            inventory,
            effective_version,
            environment,
        })
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
        profile_libraries: &'a [Library],
    },
    Quilt {
        record: &'a LoaderBuildRecord,
        profile_libraries: &'a [Library],
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
            Self::Fabric { record, .. }
            | Self::Quilt { record, .. }
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

    fn structural_libraries(&self) -> &'a [Library] {
        match self {
            Self::Fabric {
                profile_libraries, ..
            }
            | Self::Quilt {
                profile_libraries, ..
            } => profile_libraries,
            Self::Forge {
                installer_libraries,
                ..
            }
            | Self::NeoForge {
                installer_libraries,
                ..
            } => installer_libraries,
            Self::Vanilla { .. } => &[],
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
    VersionMetadataIntegrity,
    ClientIntegrity,
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
    StructuralLibraryMismatch,
    ConflictingEntry,
    ConflictingRuntimePath,
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

    if input
        .resolved_version
        .libraries
        .len()
        .saturating_add(input.shape.installer_libraries().len())
        .saturating_add(input.shape.structural_libraries().len())
        > MAX_KNOWN_GOOD_ENTRIES
    {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    let structural_proofs = structural_library_proofs(
        library_artifact_plans_for(
            input.shape.structural_libraries(),
            input.environment,
            LibraryChecksumPolicy::AllowMissing,
        )
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?,
    )?;
    let libraries = library_artifact_plans_for(
        &input.resolved_version.libraries,
        input.environment,
        LibraryChecksumPolicy::Strict,
    )
    .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    add_library_plans(&mut builder, libraries, &structural_proofs)?;
    let installer_libraries = library_artifact_plans_for(
        input.shape.installer_libraries(),
        input.environment,
        LibraryChecksumPolicy::Strict,
    )
    .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    add_library_plans(&mut builder, installer_libraries, &structural_proofs)?;

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
            if crate::loaders::api::validate_loader_build_record_identity(record).is_err()
                || record.version_id != version.id
                || loader_shape.component() != Some(record.component_id)
                || !loader_strategy_matches(record.component_id, record.strategy)
            {
                return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
            }
            let bytes = authored_loader_version_bytes(record, version)?;
            Ok(exact_bytes_integrity(&bytes))
        }
    }
}

fn authored_loader_version_bytes(
    record: &LoaderBuildRecord,
    resolved_version: &VersionJson,
) -> Result<Vec<u8>, KnownGoodInventoryError> {
    if resolved_version.materialized || !resolved_version.inherits_from.is_empty() {
        return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
    }
    let mut authored = resolved_version.clone();
    authored.inherits_from = record.minecraft_version.clone();
    authored.materialized = true;
    serde_json::to_vec_pretty(&authored).map_err(|_| KnownGoodInventoryError::MetadataSerialization)
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
    structural_proofs: &BTreeMap<ArtifactRelativePath, StructuralLibraryProof>,
) -> Result<(), KnownGoodInventoryError> {
    for plan in plans {
        let path = KnownGoodRelativePath::new(plan.relative_path.as_str())?;
        let allow_structural = if plan.expected.sha1.is_none() {
            match structural_proofs.get(&plan.relative_path) {
                Some(proof) if proof.matches(&plan) => true,
                Some(_) => return Err(KnownGoodInventoryError::StructuralLibraryMismatch),
                None => false,
            }
        } else {
            false
        };
        let integrity = expected_integrity(&plan.expected, allow_structural, path.as_str())?;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StructuralLibraryProof {
    is_native: bool,
    size: Option<u64>,
}

impl StructuralLibraryProof {
    fn from_plan(plan: &LibraryArtifactPlan) -> Self {
        Self {
            is_native: plan.is_native,
            size: plan.expected.size,
        }
    }

    fn matches(self, plan: &LibraryArtifactPlan) -> bool {
        self.is_native == plan.is_native && self.size == plan.expected.size
    }
}

fn structural_library_proofs(
    plans: Vec<LibraryArtifactPlan>,
) -> Result<BTreeMap<ArtifactRelativePath, StructuralLibraryProof>, KnownGoodInventoryError> {
    let mut proofs = BTreeMap::new();
    for plan in plans
        .into_iter()
        .filter(|plan| plan.expected.sha1.is_none())
    {
        let proof = StructuralLibraryProof::from_plan(&plan);
        if let Some(existing) = proofs.insert(plan.relative_path, proof)
            && existing != proof
        {
            return Err(KnownGoodInventoryError::StructuralLibraryMismatch);
        }
    }
    Ok(proofs)
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
    let mut entries = vec![KnownGoodEntry {
        root: root.clone(),
        path: KnownGoodRelativePath::new(COMPONENT_MANIFEST_PROOF_FILE)?,
        kind: KnownGoodArtifactKind::RuntimeManifestProof,
        integrity: exact_bytes_integrity(&manifest_proof),
    }];
    entries.push(KnownGoodEntry {
        root: root.clone(),
        path: KnownGoodRelativePath::new(".axial-ready")?,
        kind: KnownGoodArtifactKind::RuntimeReadyMarker,
        integrity: exact_bytes_integrity(b"ready"),
    });

    let plan = plan_runtime_manifest_files(manifest.files);
    if plan.file_entries.is_empty() {
        return Err(KnownGoodInventoryError::MissingRuntimeDownload);
    }
    for (path, _) in plan.directory_entries {
        entries.push(KnownGoodEntry {
            root: root.clone(),
            path: KnownGoodRelativePath::new(&path)?,
            kind: KnownGoodArtifactKind::RuntimeDirectory,
            integrity: KnownGoodIntegrity::Directory,
        });
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
        entries.push(KnownGoodEntry {
            root: root.clone(),
            path: KnownGoodRelativePath::new(&path)?,
            kind: if file.executable {
                KnownGoodArtifactKind::RuntimeExecutable
            } else {
                KnownGoodArtifactKind::RuntimeFile
            },
            integrity: expected_integrity(&expected, false, &path)?,
        });
    }
    for (path, file) in plan.link_entries {
        let target = file
            .target
            .ok_or(KnownGoodInventoryError::UnsupportedRuntimeEntry)?;
        entries.push(KnownGoodEntry {
            root: root.clone(),
            path: KnownGoodRelativePath::new(&path)?,
            kind: KnownGoodArtifactKind::RuntimeLink,
            integrity: KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget::new(&path, &target)?),
        });
    }
    if !plan.other_entries.is_empty() {
        return Err(KnownGoodInventoryError::UnsupportedRuntimeEntry);
    }
    validate_runtime_path_tree(&entries)?;
    for entry in entries {
        builder.insert(entry)?;
    }
    Ok(())
}

fn validate_runtime_path_tree(entries: &[KnownGoodEntry]) -> Result<(), KnownGoodInventoryError> {
    let mut entries_by_path = BTreeMap::new();
    for entry in entries {
        let key = (&entry.root, entry.path.as_str());
        if let Some(existing) = entries_by_path.insert(key, entry)
            && existing != entry
        {
            return Err(KnownGoodInventoryError::ConflictingRuntimePath);
        }
    }

    for entry in entries {
        for (separator, _) in entry.path.as_str().match_indices('/') {
            let ancestor_path = &entry.path.as_str()[..separator];
            if entries_by_path
                .get(&(&entry.root, ancestor_path))
                .is_some_and(|ancestor| ancestor.integrity != KnownGoodIntegrity::Directory)
            {
                return Err(KnownGoodInventoryError::ConflictingRuntimePath);
            }
        }
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
    use crate::download::{LibraryVerificationIntegrity, library_verification_plans_for};
    use crate::launch::{
        ArgumentsSection, AssetIndex, Downloads, JavaVersion, LibraryArtifact, LibraryDownload,
        LoggingConf, LoggingEntry, LoggingFile,
    };
    use crate::loaders::types::LoaderBuildSubjectKind;
    use crate::loaders::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderInstallSource, LoaderInstallability,
        build_id_for, installed_version_id_for,
    };
    use std::collections::HashMap;

    fn structural_inventory(
        root: KnownGoodRoot,
        path: &str,
        kind: KnownGoodArtifactKind,
        integrity: KnownGoodIntegrity,
    ) -> KnownGoodInventory {
        let mut builder = InventoryBuilder::default();
        builder
            .insert(KnownGoodEntry {
                root,
                path: KnownGoodRelativePath::new(path).expect("safe inventory path"),
                kind,
                integrity,
            })
            .expect("unique inventory entry");
        builder.finish()
    }

    #[test]
    fn structural_library_authority_matches_exact_root_path_kind_size_and_integrity() {
        let path = ArtifactRelativePath::new("org/example/exact/1.0/exact-1.0.jar")
            .expect("safe artifact path");
        let library = structural_inventory(
            KnownGoodRoot::Libraries,
            path.as_str(),
            KnownGoodArtifactKind::Library,
            KnownGoodIntegrity::StructuralJar { size: Some(42) },
        );
        assert!(library.authorizes_structural_library(&path, false, Some(42)));
        assert!(!library.authorizes_structural_library(&path, true, Some(42)));
        assert!(!library.authorizes_structural_library(&path, false, None));
        assert!(!library.authorizes_structural_library(&path, false, Some(41)));

        let other_path = ArtifactRelativePath::new("org/example/exact/1.0/other.jar")
            .expect("safe artifact path");
        assert!(!library.authorizes_structural_library(&other_path, false, Some(42)));

        let native = structural_inventory(
            KnownGoodRoot::Libraries,
            path.as_str(),
            KnownGoodArtifactKind::NativeLibrary,
            KnownGoodIntegrity::StructuralJar { size: None },
        );
        assert!(native.authorizes_structural_library(&path, true, None));
        assert!(!native.authorizes_structural_library(&path, false, None));
        assert!(!native.authorizes_structural_library(&path, true, Some(42)));

        for inventory in [
            structural_inventory(
                KnownGoodRoot::Assets,
                path.as_str(),
                KnownGoodArtifactKind::Library,
                KnownGoodIntegrity::StructuralJar { size: Some(42) },
            ),
            structural_inventory(
                KnownGoodRoot::Libraries,
                path.as_str(),
                KnownGoodArtifactKind::Library,
                KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata("0123456789abcdef0123456789abcdef01234567")
                        .expect("digest"),
                    size: Some(42),
                },
            ),
            structural_inventory(
                KnownGoodRoot::Libraries,
                path.as_str(),
                KnownGoodArtifactKind::Library,
                KnownGoodIntegrity::ExactBytes {
                    digest: Sha1Digest::from_metadata("0123456789abcdef0123456789abcdef01234567")
                        .expect("digest"),
                    size: 42,
                },
            ),
        ] {
            assert!(!inventory.authorizes_structural_library(&path, false, Some(42)));
        }
    }

    #[test]
    fn verification_planner_requires_exact_inventory_for_checksumless_library() {
        let relative_path = "org/example/exact/1.0/exact-1.0.jar";
        let library = Library {
            name: "org.example:exact:1.0".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: relative_path.to_string(),
                    size: 42,
                    ..LibraryArtifact::default()
                }),
                ..LibraryDownload::default()
            }),
            ..Library::default()
        };
        let inventory = structural_inventory(
            KnownGoodRoot::Libraries,
            relative_path,
            KnownGoodArtifactKind::Library,
            KnownGoodIntegrity::StructuralJar { size: Some(42) },
        );
        let root =
            std::env::temp_dir().join(format!("axial-known-good-authority-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("authority root");
        let authority =
            KnownGoodInventoryAuthority::bind(Arc::new(inventory), &root).expect("bound authority");

        let strict = library_verification_plans_for(
            &root,
            std::slice::from_ref(&library),
            &crate::rules::default_environment(),
            None,
        )
        .expect("strict plan");
        assert_eq!(
            strict[0].integrity,
            LibraryVerificationIntegrity::MissingChecksum
        );

        let authorized = library_verification_plans_for(
            &root,
            std::slice::from_ref(&library),
            &crate::rules::default_environment(),
            Some(&authority),
        )
        .expect("authorized plan");
        assert!(matches!(
            &authorized[0].integrity,
            LibraryVerificationIntegrity::StructuralJar(verification)
                if verification.expected_size == Some(42)
        ));

        let wrong_root = library_verification_plans_for(
            &root.join("other"),
            &[library],
            &crate::rules::default_environment(),
            Some(&authority),
        )
        .expect("wrong-root plan");
        assert_eq!(
            wrong_root[0].integrity,
            LibraryVerificationIntegrity::MissingChecksum
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn profile_receipt_inventory_keeps_only_the_final_shadowing_library() {
        let fixture = fixture(FixtureShape::Fabric, false);
        let record = fixture.loader_record.as_ref().expect("loader record");
        let profile_library = Library {
            name: "org.ow2.asm:asm:9.9".to_string(),
            url: "https://example.invalid/maven/".to_string(),
            size: 12,
            ..Library::default()
        };
        let mut child = fixture.version.clone();
        child.inherits_from = record.minecraft_version.clone();
        child.materialized = true;
        child.libraries = vec![profile_library.clone()];
        let mut base_version = child.clone();
        base_version.id = record.minecraft_version.clone();
        base_version.inherits_from.clear();
        base_version.materialized = false;
        base_version.libraries = vec![checksum_library(
            "org.ow2.asm:asm:9.6",
            "org/ow2/asm/asm/9.6/asm-9.6.jar",
            SHA_A,
            10,
        )];
        let base = KnownGoodInstallReceipt {
            version_id: KnownGoodId::new(&record.minecraft_version).expect("base id"),
            inventory: InventoryBuilder::default().finish(),
            effective_version: base_version,
            environment: fixture.environment.clone(),
        };
        let version_bytes = serde_json::to_vec_pretty(&child).expect("version bytes");
        let metadata = installed_loader_metadata_bytes(record).expect("loader metadata");

        let receipt = KnownGoodInstallReceipt::from_verified_profile_source(
            &base,
            record,
            child,
            &version_bytes,
            &[profile_library],
            &metadata,
        )
        .expect("profile receipt");
        let inventory = receipt.into_inventory();

        assert!(inventory.entries().iter().any(|entry| {
            entry.path().as_str() == "org/ow2/asm/asm/9.9/asm-9.9.jar"
                && matches!(
                    entry.integrity(),
                    KnownGoodIntegrity::StructuralJar { size: Some(12) }
                )
        }));
        assert!(
            !inventory
                .entries()
                .iter()
                .any(|entry| entry.path().as_str().contains("/asm/9.6/"))
        );
    }

    #[test]
    fn legacy_archive_receipt_is_derived_from_exact_child_sources_and_base_authority() {
        let fixture = fixture(FixtureShape::Vanilla, false);
        let mut record = LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id: LoaderComponentId::Forge,
            component_name: "Forge".to_string(),
            build_id: String::new(),
            minecraft_version: fixture.version.id.clone(),
            loader_version: "3.4.9.171".to_string(),
            version_id: String::new(),
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::ForgeEarliestLegacy,
            artifact_kind: LoaderArtifactKind::LegacyArchive,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::LegacyArchive {
                url: "https://example.invalid/legacy.jar".to_string(),
            },
        };
        record.build_id = build_id_for(
            record.component_id,
            &record.minecraft_version,
            &record.loader_version,
        );
        record.version_id = installed_version_id_for(
            record.component_id,
            &record.minecraft_version,
            &record.loader_version,
        )
        .expect("canonical child id");
        let base = KnownGoodInstallReceipt {
            version_id: KnownGoodId::new(&fixture.version.id).expect("base id"),
            inventory: derive_known_good_inventory(fixture.input()).expect("base inventory"),
            effective_version: fixture.version.clone(),
            environment: fixture.environment.clone(),
        };
        let child_client_bytes = b"deterministic legacy child bytes";
        let mut child = fixture.version.clone();
        child.id = record.version_id.clone();
        child.inherits_from = record.minecraft_version.clone();
        child.materialized = true;
        let child_download = child.downloads.client.as_mut().expect("client download");
        child_download.sha1 = sha1_digest(child_client_bytes).as_str().to_string();
        child_download.size = child_client_bytes.len() as i64;
        child_download.url.clear();
        let version_bytes = serde_json::to_vec_pretty(&child).expect("version bytes");
        let metadata = installed_loader_metadata_bytes(&record).expect("loader metadata");

        let receipt = KnownGoodInstallReceipt::from_verified_legacy_archive_source(
            &base,
            &record,
            child,
            &version_bytes,
            child_client_bytes,
            &metadata,
        )
        .expect("legacy receipt");
        let inventory = receipt.into_inventory();

        assert_entry(
            &inventory,
            &KnownGoodRoot::Versions,
            &format!("{0}/{0}.jar", record.version_id),
            KnownGoodArtifactKind::ClientJar,
            &KnownGoodIntegrity::Sha1 {
                digest: sha1_digest(child_client_bytes),
                size: Some(child_client_bytes.len() as u64),
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Versions,
            &format!("{0}/{0}.json", record.version_id),
            KnownGoodArtifactKind::VersionMetadata,
            &exact_bytes_integrity(&version_bytes),
        );
        assert!(has_kind(&inventory, KnownGoodArtifactKind::AssetIndex));
        assert!(has_kind(
            &inventory,
            KnownGoodArtifactKind::RuntimeExecutable
        ));
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "com/mojang/strict/1.0/strict-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("library digest"),
                size: Some(10),
            },
        );
        assert!(!inventory.entries().iter().any(|entry| {
            entry.root() == &KnownGoodRoot::Versions
                && entry.path().as_str().starts_with(&fixture.version.id)
        }));
        assert_sorted_unique(&inventory);
    }

    #[test]
    fn authenticated_base_receipt_rejects_corrupt_client_bytes() {
        let fixture = fixture(FixtureShape::Vanilla, false);
        let base = KnownGoodInstallReceipt {
            version_id: KnownGoodId::new(&fixture.version.id).expect("base id"),
            inventory: InventoryBuilder::default().finish(),
            effective_version: fixture.version,
            environment: fixture.environment,
        };

        assert_eq!(
            base.authenticate_client_bytes(b"not the authenticated client"),
            Err(KnownGoodInventoryError::ClientIntegrity)
        );
    }

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
    fn runtime_path_tree_allows_directory_ancestor() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_directory_entry("bin"),
            runtime_file_entry("bin/java"),
        ]));

        let inventory = derive_known_good_inventory(fixture.input()).expect("valid runtime tree");

        assert_entry(
            &inventory,
            &runtime_root(),
            "bin",
            KnownGoodArtifactKind::RuntimeDirectory,
            &KnownGoodIntegrity::Directory,
        );
        assert!(has_kind(&inventory, KnownGoodArtifactKind::RuntimeFile));
    }

    #[test]
    fn runtime_path_tree_rejects_file_ancestor() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_file_entry("bin"),
            runtime_file_entry("bin/java"),
        ]));

        let error = derive_known_good_inventory(fixture.input()).expect_err("file ancestor");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
    }

    #[test]
    fn runtime_path_tree_rejects_link_ancestor() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_link_entry("bin", "java"),
            runtime_file_entry("bin/java"),
        ]));

        let error = derive_known_good_inventory(fixture.input()).expect_err("link ancestor");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
    }

    #[test]
    fn runtime_path_tree_rejects_incompatible_exact_path() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[runtime_file_entry(
            ".axial-ready",
        )]));

        let error = derive_known_good_inventory(fixture.input()).expect_err("reserved path");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
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
    fn loader_shape_does_not_authorize_checksumless_base_library() {
        let mut fixture = fixture(FixtureShape::Fabric, false);
        fixture.version.libraries[0]
            .downloads
            .as_mut()
            .and_then(|downloads| downloads.artifact.as_mut())
            .expect("base library artifact")
            .sha1
            .clear();

        let error = derive_known_good_inventory(fixture.input())
            .expect_err("checksumless base library must remain strict");
        assert_eq!(error, KnownGoodInventoryError::MissingChecksum);
    }

    #[test]
    fn structural_library_source_size_must_match_resolved_library() {
        let mut fixture = fixture(FixtureShape::Fabric, false);
        fixture
            .version
            .libraries
            .iter_mut()
            .find(|library| library.name == "net.loader:loader-unverified:1.0")
            .expect("checksumless resolved library")
            .size = 13;

        let error = derive_known_good_inventory(fixture.input())
            .expect_err("source size mismatch must not mint structural authority");

        assert_eq!(error, KnownGoodInventoryError::StructuralLibraryMismatch);
    }

    #[test]
    fn structural_library_source_kind_must_match_resolved_library() {
        let path = ArtifactRelativePath::new("net/loader/exact/1.0/exact-1.0.jar")
            .expect("canonical library path");
        let proofs =
            structural_library_proofs(vec![structural_plan(path.clone(), false, Some(12))])
                .expect("source proof");
        let mut builder = InventoryBuilder::default();

        let error = add_library_plans(
            &mut builder,
            vec![structural_plan(path, true, Some(12))],
            &proofs,
        )
        .expect_err("source kind mismatch must not mint structural authority");

        assert_eq!(error, KnownGoodInventoryError::StructuralLibraryMismatch);
    }

    #[test]
    fn conflicting_structural_library_source_proofs_fail_closed() {
        let path = ArtifactRelativePath::new("net/loader/exact/1.0/exact-1.0.jar")
            .expect("canonical library path");

        let error = structural_library_proofs(vec![
            structural_plan(path.clone(), false, Some(12)),
            structural_plan(path, false, None),
        ])
        .expect_err("conflicting source proofs");

        assert_eq!(error, KnownGoodInventoryError::StructuralLibraryMismatch);
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
    fn noncanonical_loader_version_identity_cannot_mint_inventory() {
        let mut fixture = fixture(FixtureShape::Fabric, false);
        fixture.version.id = "noncanonical-loader-id".to_string();
        fixture.loader_record.as_mut().unwrap().version_id = fixture.version.id.clone();

        let error =
            derive_known_good_inventory(fixture.input()).expect_err("noncanonical loader identity");
        assert_eq!(error, KnownGoodInventoryError::LoaderIdentityMismatch);
    }

    #[test]
    fn noncanonical_loader_build_identity_cannot_mint_inventory() {
        let mut fixture = fixture(FixtureShape::Quilt, false);
        fixture.loader_record.as_mut().unwrap().build_id.push('A');

        let error = derive_known_good_inventory(fixture.input())
            .expect_err("noncanonical loader build identity");
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
        let version_base = &record.version_id;
        let expected = installed_loader_metadata_bytes(record).unwrap();
        assert_entry(
            inventory,
            &KnownGoodRoot::Versions,
            &format!("{version_base}/.axial-loader.json"),
            KnownGoodArtifactKind::LoaderMetadata,
            &exact_bytes_integrity(&expected),
        );

        let authored = authored_loader_version_bytes(record, &fixture.version).unwrap();
        let resolved = serde_json::to_vec_pretty(&fixture.version).unwrap();
        assert_ne!(authored, resolved);
        assert_entry(
            inventory,
            &KnownGoodRoot::Versions,
            &format!("{version_base}/{version_base}.json"),
            KnownGoodArtifactKind::VersionMetadata,
            &exact_bytes_integrity(&authored),
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
        profile_libraries: Vec<Library>,
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
                    profile_libraries: &self.profile_libraries,
                },
                FixtureShape::Quilt => KnownGoodInstallShape::Quilt {
                    record: record.expect("quilt record"),
                    profile_libraries: &self.profile_libraries,
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

        fn replace_runtime_manifest(&mut self, bytes: Vec<u8>) {
            self.runtime_manifest_expected = expected_for(&bytes);
            self.runtime_manifest_bytes = bytes;
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
        let profile_libraries = matches!(shape, FixtureShape::Fabric | FixtureShape::Quilt)
            .then(|| vec![checksumless_loader_library()])
            .unwrap_or_default();
        let mut installer_libraries =
            if matches!(shape, FixtureShape::Forge | FixtureShape::NeoForge) {
                vec![
                    checksum_library(
                        "net.example:installer-only:1.0",
                        "net/example/installer-only/1.0/installer-only-1.0.jar",
                        SHA_C,
                        30,
                    ),
                    checksumless_loader_library(),
                ]
            } else {
                Vec::new()
            };
        libraries.extend(profile_libraries.iter().cloned());
        if matches!(shape, FixtureShape::Forge | FixtureShape::NeoForge) {
            libraries.push(checksumless_loader_library());
        }
        if shuffled {
            libraries.reverse();
            installer_libraries.reverse();
        }
        let loader_record = shape
            .component()
            .map(|component| loader_record(shape, component));
        let version_id = loader_record
            .as_ref()
            .map(|record| record.version_id.clone())
            .unwrap_or_else(|| "fixture-version".to_string());
        let version = VersionJson {
            id: version_id.clone(),
            inherits_from: String::new(),
            materialized: false,
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
        Fixture {
            version,
            version_manifest: ManifestEntry {
                id: version_id,
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
            profile_libraries,
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
            build_id: build_id_for(component, "1.21.1", "1.0"),
            minecraft_version: "1.21.1".to_string(),
            loader_version: "1.0".to_string(),
            version_id: installed_version_id_for(component, "1.21.1", "1.0")
                .expect("canonical fixture loader version id"),
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

    fn structural_plan(
        relative_path: ArtifactRelativePath,
        is_native: bool,
        size: Option<u64>,
    ) -> LibraryArtifactPlan {
        LibraryArtifactPlan {
            name: "exact structural library".to_string(),
            source_url: Some("https://example.invalid/library".to_string()),
            relative_path,
            expected: ExpectedIntegrity { size, sha1: None },
            allow_missing_checksum: true,
            is_native,
        }
    }

    fn runtime_manifest_bytes(shuffled: bool) -> Vec<u8> {
        let executable = runtime_executable_entry("bin/java");
        let directory = runtime_directory_entry("bin");
        let link = runtime_link_entry("java-link", "./bin/../bin/java");
        let files = if shuffled {
            format!("{link},{executable},{directory}")
        } else {
            format!("{directory},{executable},{link}")
        };
        format!(r#"{{"files":{{{files}}}}}"#).into_bytes()
    }

    fn runtime_manifest_with_entries(entries: &[String]) -> Vec<u8> {
        format!(r#"{{"files":{{{}}}}}"#, entries.join(",")).into_bytes()
    }

    fn runtime_directory_entry(path: &str) -> String {
        format!(r#""{path}":{{"type":"directory"}}"#)
    }

    fn runtime_file_entry(path: &str) -> String {
        format!(
            r#""{path}":{{"type":"file","downloads":{{"raw":{{"url":"https://example.invalid/file","sha1":"{SHA_B}","size":20}}}}}}"#
        )
    }

    fn runtime_executable_entry(path: &str) -> String {
        format!(
            r#""{path}":{{"type":"file","executable":true,"downloads":{{"raw":{{"url":"https://example.invalid/java","sha1":"{SHA_B}","size":20}},"lzma":{{"url":"https://example.invalid/java.lzma","sha1":"{SHA_C}","size":10}}}}}}"#
        )
    }

    fn runtime_link_entry(path: &str, target: &str) -> String {
        format!(r#""{path}":{{"type":"link","target":"{target}"}}"#)
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
