use crate::portable_path::{
    PortableRelativePath, MAX_PORTABLE_FILE_NAME_BYTES, MAX_PORTABLE_RELATIVE_PATH_BYTES,
};
#[cfg(feature = "test-support")]
use crate::download::AssetSourcePool;
use crate::download::library_source::{
    AuthenticatedLibraryCacheProofSet, RetainedLibraryComponentSource, RetainedLibrarySourceSet,
};
use crate::download::{
    ASSET_OBJECT_BASE_URL, AuthenticatedAssetCacheProofSet, AuthenticatedSelectedArtifactSource,
    AuthenticatedVanillaInstallSources, AuthenticatedVersionBundleSource, DownloadError,
    ExpectedIntegrity, LibraryArtifactPlan, ReconstructedVanillaAuthority,
    ReconstructedVanillaAuthorityParts, RetainedAssetComponentSource, RetainedAssetSourceSet,
    RetainedVersionBundleReconstructionSources, SelectedDownloadArtifactKind,
    library_artifact_plans_for, parse_asset_index,
};
use crate::known_good_libraries::{SealedExactLibraryDeclarations, SealedLibraryKind};
use crate::launch::{Library, VersionJson, effective_java_version_for};
use crate::loaders::{
    AuthenticatedInstallerReceiptInput, AuthenticatedInstallerReconstructionAuthority,
    AuthenticatedLegacyOverlayAuthority, LoaderBuildRecord, LoaderComponentId, LoaderInstallSource,
    LoaderInstallStrategy, VerifiedInstallerClientBytes, VerifiedInstallerReceiptSource,
    compose_loader_version,
};
#[cfg(feature = "test-support")]
use crate::managed_component_table::ManagedComponentArtifactKind;
use crate::managed_fs::ManagedDir;
#[cfg(test)]
use crate::manifest::ManifestEntry;
use crate::rules::Environment;
use crate::runtime::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, RuntimeId, RuntimeSourceReceipt,
    component_manifest_proof_bytes, plan_runtime_manifest_files, preferred_runtime_component,
};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const MAX_KNOWN_GOOD_RELATIVE_PATH_BYTES: usize = MAX_PORTABLE_RELATIVE_PATH_BYTES;
pub const MAX_KNOWN_GOOD_PATH_SEGMENT_BYTES: usize = MAX_PORTABLE_FILE_NAME_BYTES;
pub const MAX_KNOWN_GOOD_ENTRIES: usize = 200_000;
pub const MAX_KNOWN_GOOD_VERSION_JSON_BYTES: usize = 16 << 20;
pub const MAX_KNOWN_GOOD_ASSET_INDEX_BYTES: usize = 64 << 20;
pub const MAX_KNOWN_GOOD_RUNTIME_MANIFEST_BYTES: usize = 16 << 20;
pub const MAX_LAUNCH_TIER0_ENTRIES: usize = 512;
pub const MAX_LAUNCH_TIER1_ENTRIES: usize = 512;
pub const MAX_LAUNCH_TIER1_ARTIFACT_BYTES: u64 = 512 << 20;
pub const MAX_LAUNCH_TIER1_AGGREGATE_BYTES: u64 = 2 << 30;
pub const MAX_TIER2_ENTRIES: usize = MAX_KNOWN_GOOD_ENTRIES;
pub const MAX_TIER2_ARTIFACT_BYTES: u64 = 512 << 20;
pub const MAX_TIER2_AGGREGATE_BYTES: u64 = 16 << 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchTier0RuntimeSelection<'a> {
    PreferredManaged,
    ManagedComponent(&'a str),
    ExternalExecutable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedKnownGoodComponent {
    VersionBundle,
    Libraries,
    Assets,
}

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
    RuntimeManifestProof,
    RuntimeReadyMarker,
    RuntimeFile,
    RuntimeExecutable,
    RuntimeDirectory,
    RuntimeLink,
}

impl KnownGoodArtifactKind {
    pub fn stable_id(self) -> &'static str {
        match self {
            Self::VersionMetadata => "version_metadata",
            Self::ClientJar => "client_jar",
            Self::Library => "library",
            Self::NativeLibrary => "native_library",
            Self::AssetIndex => "asset_index",
            Self::AssetObject => "asset_object",
            Self::LogConfig => "log_config",
            Self::RuntimeManifestProof => "runtime_manifest_proof",
            Self::RuntimeReadyMarker => "runtime_ready_marker",
            Self::RuntimeFile => "runtime_file",
            Self::RuntimeExecutable => "runtime_executable",
            Self::RuntimeDirectory => "runtime_directory",
            Self::RuntimeLink => "runtime_link",
        }
    }

    pub fn needed_for_launch_tier0(self) -> bool {
        matches!(
            self,
            Self::VersionMetadata
                | Self::ClientJar
                | Self::Library
                | Self::NativeLibrary
                | Self::AssetIndex
                | Self::LogConfig
                | Self::RuntimeManifestProof
                | Self::RuntimeReadyMarker
                | Self::RuntimeExecutable
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KnownGoodIntegrity {
    Sha1 { digest: Sha1Digest, size: u64 },
    ExactBytes { digest: Sha1Digest, size: u64 },
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

pub struct KnownGoodPhysicalPath {
    root: PathBuf,
    relative: PathBuf,
}

impl KnownGoodPhysicalPath {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn relative(&self) -> &Path {
        &self.relative
    }

    #[cfg(feature = "test-support")]
    pub fn for_test(root: PathBuf, relative: PathBuf) -> Self {
        Self { root, relative }
    }

    #[cfg(test)]
    fn absolute(&self) -> PathBuf {
        self.root.join(&self.relative)
    }
}

pub fn known_good_entry_path(
    library_root: &Path,
    runtime_cache: &crate::runtime::ManagedRuntimeCache,
    entry: &KnownGoodEntry,
) -> KnownGoodPhysicalPath {
    let (root, mut relative) = match entry.root() {
        KnownGoodRoot::Versions => (library_root.to_path_buf(), PathBuf::from("versions")),
        KnownGoodRoot::Libraries => (library_root.to_path_buf(), PathBuf::from("libraries")),
        KnownGoodRoot::Assets => (library_root.to_path_buf(), PathBuf::from("assets")),
        KnownGoodRoot::ManagedRuntime { component } => (
            runtime_cache.root().to_path_buf(),
            PathBuf::from(component.as_str()),
        ),
    };
    for segment in entry.path().as_str().split('/') {
        relative.push(segment);
    }
    KnownGoodPhysicalPath { root, relative }
}

pub fn known_good_link_target_matches(entry: &KnownGoodEntry, observed: &Path) -> bool {
    let KnownGoodIntegrity::LinkTarget(expected) = entry.integrity() else {
        return false;
    };
    observed.to_str().is_some_and(|observed| {
        KnownGoodLinkTarget::new(entry.path().as_str(), observed)
            .is_ok_and(|observed| observed == *expected)
    })
}

pub struct KnownGoodInventory {
    entries: Vec<KnownGoodEntry>,
    standalone_leaf_repair_sources: BTreeMap<usize, KnownGoodStandaloneLeafRepairSourceContract>,
}

impl fmt::Debug for KnownGoodInventory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KnownGoodInventory")
            .field("entries", &self.entries)
            .field(
                "standalone_leaf_repair_source_count",
                &self.standalone_leaf_repair_sources.len(),
            )
            .finish()
    }
}

impl PartialEq for KnownGoodInventory {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
            && self.standalone_leaf_repair_sources == other.standalone_leaf_repair_sources
    }
}

impl Eq for KnownGoodInventory {}

#[derive(Clone, Eq, PartialEq)]
struct KnownGoodStandaloneLeafRepairSourceContract {
    root: KnownGoodRoot,
    path: KnownGoodRelativePath,
    kind: KnownGoodArtifactKind,
    digest: Sha1Digest,
    size: u64,
    provider_url: String,
}

pub struct KnownGoodStandaloneLeafRepairSource<'a> {
    inventory_ordinal: usize,
    contract: &'a KnownGoodStandaloneLeafRepairSourceContract,
}

impl KnownGoodStandaloneLeafRepairSource<'_> {
    pub fn inventory_ordinal(&self) -> usize {
        self.inventory_ordinal
    }

    pub fn root(&self) -> &KnownGoodRoot {
        &self.contract.root
    }

    pub fn path(&self) -> &KnownGoodRelativePath {
        &self.contract.path
    }

    pub fn kind(&self) -> KnownGoodArtifactKind {
        self.contract.kind
    }

    pub fn sha1(&self) -> &Sha1Digest {
        &self.contract.digest
    }

    pub fn size(&self) -> u64 {
        self.contract.size
    }

    pub fn provider_url(&self) -> &str {
        &self.contract.provider_url
    }
}

impl KnownGoodInventory {
    pub fn entries(&self) -> &[KnownGoodEntry] {
        &self.entries
    }

    pub fn bind_standalone_leaf_repair_source(
        &self,
        inventory_ordinal: usize,
    ) -> Result<KnownGoodStandaloneLeafRepairSource<'_>, KnownGoodRepairSourceError> {
        let entry = self
            .entries
            .get(inventory_ordinal)
            .ok_or(KnownGoodRepairSourceError::UnknownInventoryOrdinal)?;
        let contract = self
            .standalone_leaf_repair_sources
            .get(&inventory_ordinal)
            .ok_or(KnownGoodRepairSourceError::UnsupportedInventoryEntry)?;
        if !contract.matches(entry) {
            return Err(KnownGoodRepairSourceError::ContractMismatch);
        }
        Ok(KnownGoodStandaloneLeafRepairSource {
            inventory_ordinal,
            contract,
        })
    }

    fn inherited_repair_provider_for_entry<'a>(
        &'a self,
        entry: &KnownGoodEntry,
        effective_provider_url: Option<&str>,
    ) -> Result<Option<&'a str>, KnownGoodInventoryError> {
        let Some(effective_provider_url) = effective_provider_url else {
            return Ok(None);
        };
        let inventory_ordinal = self
            .entries
            .iter()
            .position(|candidate| candidate == entry)
            .ok_or(KnownGoodInventoryError::InvalidRepairSource)?;
        let Some(contract) = self.standalone_leaf_repair_sources.get(&inventory_ordinal) else {
            return Ok(None);
        };
        if !contract.matches(entry) || contract.provider_url != effective_provider_url {
            return Err(KnownGoodInventoryError::ConflictingRepairSource);
        }
        Ok(Some(&contract.provider_url))
    }

    pub fn launch_tier0_projection(
        &self,
        runtime_selection: LaunchTier0RuntimeSelection<'_>,
    ) -> Result<Vec<(usize, &KnownGoodEntry)>, LaunchTier0ProjectionError> {
        let selected = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.kind().needed_for_launch_tier0() && runtime_selection.includes(entry.root())
            })
            .collect::<Vec<_>>();
        if selected.len() > MAX_LAUNCH_TIER0_ENTRIES {
            return Err(LaunchTier0ProjectionError {
                selected_entry_count: selected.len(),
            });
        }
        Ok(selected)
    }

    pub fn launch_tier1_projection(
        &self,
    ) -> Result<LaunchTier1Projection, LaunchTier1ProjectionError> {
        let selected_entry_count = self
            .entries
            .iter()
            .filter(|entry| launch_tier1_root(entry).is_some())
            .count();
        if selected_entry_count > MAX_LAUNCH_TIER1_ENTRIES {
            return Err(LaunchTier1ProjectionError::TooManyEntries {
                selected_entry_count,
            });
        }

        let mut expected_byte_count = 0_u64;
        let mut entries = Vec::with_capacity(selected_entry_count);
        for (inventory_ordinal, entry, root) in
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(inventory_ordinal, entry)| {
                    launch_tier1_root(entry).map(|root| (inventory_ordinal, entry, root))
                })
        {
            let (digest, size) = match entry.integrity() {
                KnownGoodIntegrity::Sha1 { digest, size }
                | KnownGoodIntegrity::ExactBytes { digest, size } => (digest.clone(), *size),
                KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                    return Err(LaunchTier1ProjectionError::UnsupportedIntegrity {
                        selected_entry_count,
                        inventory_ordinal,
                    });
                }
            };
            if size > MAX_LAUNCH_TIER1_ARTIFACT_BYTES {
                return Err(LaunchTier1ProjectionError::ArtifactByteLimitExceeded {
                    selected_entry_count,
                    inventory_ordinal,
                    expected_byte_count: size,
                });
            }
            expected_byte_count += size;
            if expected_byte_count > MAX_LAUNCH_TIER1_AGGREGATE_BYTES {
                return Err(LaunchTier1ProjectionError::AggregateByteLimitExceeded {
                    selected_entry_count,
                    expected_byte_count,
                });
            }
            entries.push(LaunchTier1ProjectionEntry {
                inventory_ordinal,
                file: LaunchTier1AdmittedFile {
                    root: entry.root.clone(),
                    physical_root: root,
                    path: entry.path.clone(),
                    kind: entry.kind,
                    digest,
                    size,
                },
            });
        }

        Ok(LaunchTier1Projection { entries })
    }

    pub fn tier2_projection(&self) -> Result<Tier2Projection<'_>, Tier2ProjectionError> {
        let entry_count = self.entries.len();
        if entry_count > MAX_TIER2_ENTRIES {
            return Err(Tier2ProjectionError::TooManyEntries { entry_count });
        }

        let mut expected_content_byte_count = 0_u64;
        for (inventory_ordinal, entry) in self.entries.iter().enumerate() {
            if !tier2_root_kind_is_supported(entry) {
                return Err(Tier2ProjectionError::UnsupportedRootKind {
                    entry_count,
                    inventory_ordinal,
                });
            }
            let Some(size) = tier2_content_size(entry) else {
                if tier2_non_file_integrity_is_supported(entry) {
                    continue;
                }
                return Err(Tier2ProjectionError::UnsupportedIntegrity {
                    entry_count,
                    inventory_ordinal,
                });
            };
            if size > MAX_TIER2_ARTIFACT_BYTES {
                return Err(Tier2ProjectionError::ArtifactByteLimitExceeded {
                    entry_count,
                    inventory_ordinal,
                    expected_byte_count: size,
                });
            }
            expected_content_byte_count = expected_content_byte_count.saturating_add(size);
            if expected_content_byte_count > MAX_TIER2_AGGREGATE_BYTES {
                return Err(Tier2ProjectionError::AggregateByteLimitExceeded {
                    entry_count,
                    expected_byte_count: expected_content_byte_count,
                });
            }
        }

        Ok(Tier2Projection {
            entries: &self.entries,
            expected_content_byte_count,
        })
    }

    pub fn managed_component_projection(
        &self,
        component: ManagedKnownGoodComponent,
    ) -> Result<ManagedComponentProjection<'_>, ManagedComponentProjectionError> {
        let selected_entry_count = self
            .entries
            .iter()
            .filter(|entry| managed_component_for_kind(entry.kind()) == Some(component))
            .count();
        if selected_entry_count > MAX_TIER2_ENTRIES {
            return Err(ManagedComponentProjectionError::TooManyEntries {
                selected_entry_count,
            });
        }

        let mut expected_content_byte_count = 0_u64;
        let mut entries = Vec::with_capacity(selected_entry_count);
        for (inventory_ordinal, entry) in self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| managed_component_for_kind(entry.kind()) == Some(component))
        {
            if !managed_component_root_kind_is_supported(component, entry) {
                return Err(ManagedComponentProjectionError::UnsupportedRootKind {
                    selected_entry_count,
                    inventory_ordinal,
                });
            }
            let size = match entry.integrity() {
                KnownGoodIntegrity::Sha1 { size, .. }
                | KnownGoodIntegrity::ExactBytes { size, .. } => *size,
                KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                    return Err(ManagedComponentProjectionError::UnsupportedIntegrity {
                        selected_entry_count,
                        inventory_ordinal,
                    });
                }
            };
            if size > MAX_TIER2_ARTIFACT_BYTES {
                return Err(ManagedComponentProjectionError::ArtifactByteLimitExceeded {
                    selected_entry_count,
                    inventory_ordinal,
                    expected_byte_count: size,
                });
            }
            expected_content_byte_count = expected_content_byte_count.checked_add(size).ok_or(
                ManagedComponentProjectionError::AggregateByteCountOverflow {
                    selected_entry_count,
                    inventory_ordinal,
                },
            )?;
            if expected_content_byte_count > MAX_TIER2_AGGREGATE_BYTES {
                return Err(
                    ManagedComponentProjectionError::AggregateByteLimitExceeded {
                        selected_entry_count,
                        expected_byte_count: expected_content_byte_count,
                    },
                );
            }
            entries.push(ManagedComponentProjectionEntry {
                inventory_ordinal,
                entry,
            });
        }

        entries.sort_unstable_by(|left, right| {
            left.entry
                .root()
                .cmp(right.entry.root())
                .then_with(|| left.entry.path().cmp(right.entry.path()))
                .then_with(|| left.inventory_ordinal.cmp(&right.inventory_ordinal))
        });
        validate_managed_component_path_tree(selected_entry_count, &entries)?;

        Ok(ManagedComponentProjection {
            component,
            entries,
            expected_content_byte_count,
        })
    }

    #[cfg(feature = "test-support")]
    pub fn from_test_entries(
        entries: impl IntoIterator<Item = TestKnownGoodEntry>,
    ) -> Result<Self, KnownGoodInventoryError> {
        let mut builder = InventoryBuilder::default();
        for entry in entries {
            let root = match entry.root {
                TestKnownGoodRoot::Versions => KnownGoodRoot::Versions,
                TestKnownGoodRoot::Libraries => KnownGoodRoot::Libraries,
                TestKnownGoodRoot::Assets => KnownGoodRoot::Assets,
                TestKnownGoodRoot::ManagedRuntime { component } => KnownGoodRoot::ManagedRuntime {
                    component: KnownGoodId::new(&component)?,
                },
            };
            let integrity = match entry.integrity {
                TestKnownGoodIntegrity::File { size } => KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata("0000000000000000000000000000000000000000")?,
                    size,
                },
                TestKnownGoodIntegrity::Sha1 { digest, size } => KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata(&digest)?,
                    size,
                },
                TestKnownGoodIntegrity::ExactBytes { size } => KnownGoodIntegrity::ExactBytes {
                    digest: Sha1Digest::from_metadata("0000000000000000000000000000000000000000")?,
                    size,
                },
                TestKnownGoodIntegrity::Directory => KnownGoodIntegrity::Directory,
                TestKnownGoodIntegrity::LinkTarget(target) => {
                    KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget::new(&entry.path, &target)?)
                }
            };
            builder.insert(KnownGoodEntry {
                root,
                path: KnownGoodRelativePath::new(&entry.path)?,
                kind: entry.kind,
                integrity,
            })?;
        }
        Ok(builder.finish())
    }

    #[cfg(feature = "test-support")]
    pub fn with_test_standalone_leaf_repair_source(
        mut self,
        inventory_ordinal: usize,
        provider_url: &str,
    ) -> Result<Self, KnownGoodInventoryError> {
        let entry = self
            .entries
            .get(inventory_ordinal)
            .ok_or(KnownGoodInventoryError::InvalidRepairSource)?;
        let contract = KnownGoodStandaloneLeafRepairSourceContract::new(entry, provider_url)?;
        if self
            .standalone_leaf_repair_sources
            .insert(inventory_ordinal, contract)
            .is_some()
        {
            return Err(KnownGoodInventoryError::ConflictingRepairSource);
        }
        Ok(self)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn version_bundle_for_test(
        version_id: &str,
        version_json: &[u8],
        client_jar: &[u8],
        log_config: Option<(&str, &[u8])>,
    ) -> Self {
        let mut builder = InventoryBuilder::default();
        let mut insert = |root, path: String, kind, bytes: &[u8]| {
            builder
                .insert(KnownGoodEntry {
                    root,
                    path: KnownGoodRelativePath::new(&path).expect("test version bundle path"),
                    kind,
                    integrity: KnownGoodIntegrity::Sha1 {
                        digest: Sha1Digest::from_metadata(&format!("{:x}", Sha1::digest(bytes)))
                            .expect("test version bundle digest"),
                        size: u64::try_from(bytes.len()).expect("test version bundle size"),
                    },
                })
                .expect("unique test version bundle entry");
        };
        insert(
            KnownGoodRoot::Versions,
            format!("{version_id}/{version_id}.json"),
            KnownGoodArtifactKind::VersionMetadata,
            version_json,
        );
        insert(
            KnownGoodRoot::Versions,
            format!("{version_id}/{version_id}.jar"),
            KnownGoodArtifactKind::ClientJar,
            client_jar,
        );
        if let Some((name, bytes)) = log_config {
            insert(
                KnownGoodRoot::Assets,
                format!("log_configs/{name}"),
                KnownGoodArtifactKind::LogConfig,
                bytes,
            );
        }
        builder.finish()
    }
}

fn managed_component_for_kind(kind: KnownGoodArtifactKind) -> Option<ManagedKnownGoodComponent> {
    match kind {
        KnownGoodArtifactKind::VersionMetadata
        | KnownGoodArtifactKind::ClientJar
        | KnownGoodArtifactKind::LogConfig => Some(ManagedKnownGoodComponent::VersionBundle),
        KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary => {
            Some(ManagedKnownGoodComponent::Libraries)
        }
        KnownGoodArtifactKind::AssetIndex | KnownGoodArtifactKind::AssetObject => {
            Some(ManagedKnownGoodComponent::Assets)
        }
        KnownGoodArtifactKind::RuntimeManifestProof
        | KnownGoodArtifactKind::RuntimeReadyMarker
        | KnownGoodArtifactKind::RuntimeFile
        | KnownGoodArtifactKind::RuntimeExecutable
        | KnownGoodArtifactKind::RuntimeDirectory
        | KnownGoodArtifactKind::RuntimeLink => None,
    }
}

fn managed_component_root_kind_is_supported(
    component: ManagedKnownGoodComponent,
    entry: &KnownGoodEntry,
) -> bool {
    matches!(
        (component, entry.root(), entry.kind()),
        (
            ManagedKnownGoodComponent::VersionBundle,
            KnownGoodRoot::Versions,
            KnownGoodArtifactKind::VersionMetadata | KnownGoodArtifactKind::ClientJar,
        ) | (
            ManagedKnownGoodComponent::VersionBundle,
            KnownGoodRoot::Assets,
            KnownGoodArtifactKind::LogConfig,
        ) | (
            ManagedKnownGoodComponent::Libraries,
            KnownGoodRoot::Libraries,
            KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary,
        ) | (
            ManagedKnownGoodComponent::Assets,
            KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetIndex | KnownGoodArtifactKind::AssetObject,
        )
    )
}

fn validate_managed_component_path_tree(
    selected_entry_count: usize,
    entries: &[ManagedComponentProjectionEntry<'_>],
) -> Result<(), ManagedComponentProjectionError> {
    let mut entries_by_path = BTreeMap::new();
    for projected in entries {
        let key = (projected.entry.root(), projected.entry.path().as_str());
        if let Some(first_inventory_ordinal) =
            entries_by_path.insert(key, projected.inventory_ordinal)
        {
            return Err(ManagedComponentProjectionError::PathCollision {
                selected_entry_count,
                first_inventory_ordinal,
                second_inventory_ordinal: projected.inventory_ordinal,
            });
        }
    }

    for projected in entries {
        for (separator, _) in projected.entry.path().as_str().match_indices('/') {
            let ancestor_path = &projected.entry.path().as_str()[..separator];
            if let Some(first_inventory_ordinal) = entries_by_path
                .get(&(projected.entry.root(), ancestor_path))
                .copied()
            {
                return Err(ManagedComponentProjectionError::PathCollision {
                    selected_entry_count,
                    first_inventory_ordinal,
                    second_inventory_ordinal: projected.inventory_ordinal,
                });
            }
        }
    }
    Ok(())
}

fn launch_tier1_root(entry: &KnownGoodEntry) -> Option<LaunchTier1PhysicalRoot> {
    match (entry.root(), entry.kind()) {
        (KnownGoodRoot::Versions, KnownGoodArtifactKind::ClientJar) => {
            Some(LaunchTier1PhysicalRoot::Versions)
        }
        (
            KnownGoodRoot::Libraries,
            KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary,
        ) => Some(LaunchTier1PhysicalRoot::Libraries),
        _ => None,
    }
}

fn tier2_root_kind_is_supported(entry: &KnownGoodEntry) -> bool {
    matches!(
        (entry.root(), entry.kind()),
        (
            KnownGoodRoot::Versions,
            KnownGoodArtifactKind::VersionMetadata | KnownGoodArtifactKind::ClientJar,
        ) | (
            KnownGoodRoot::Libraries,
            KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary,
        ) | (
            KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetIndex
                | KnownGoodArtifactKind::AssetObject
                | KnownGoodArtifactKind::LogConfig,
        ) | (
            KnownGoodRoot::ManagedRuntime { .. },
            KnownGoodArtifactKind::RuntimeManifestProof
                | KnownGoodArtifactKind::RuntimeReadyMarker
                | KnownGoodArtifactKind::RuntimeFile
                | KnownGoodArtifactKind::RuntimeExecutable
                | KnownGoodArtifactKind::RuntimeDirectory
                | KnownGoodArtifactKind::RuntimeLink,
        )
    )
}

fn tier2_content_size(entry: &KnownGoodEntry) -> Option<u64> {
    match entry.integrity() {
        KnownGoodIntegrity::Sha1 { size, .. } | KnownGoodIntegrity::ExactBytes { size, .. }
            if !matches!(
                entry.kind(),
                KnownGoodArtifactKind::RuntimeDirectory | KnownGoodArtifactKind::RuntimeLink
            ) =>
        {
            Some(*size)
        }
        _ => None,
    }
}

fn tier2_non_file_integrity_is_supported(entry: &KnownGoodEntry) -> bool {
    matches!(
        (entry.kind(), entry.integrity()),
        (
            KnownGoodArtifactKind::RuntimeDirectory,
            KnownGoodIntegrity::Directory,
        ) | (
            KnownGoodArtifactKind::RuntimeLink | KnownGoodArtifactKind::RuntimeExecutable,
            KnownGoodIntegrity::LinkTarget(_),
        )
    )
}

impl LaunchTier0RuntimeSelection<'_> {
    fn includes(self, root: &KnownGoodRoot) -> bool {
        let KnownGoodRoot::ManagedRuntime { component } = root else {
            return true;
        };
        match self {
            Self::PreferredManaged => true,
            Self::ManagedComponent(selected) => component.as_str() == selected,
            Self::ExternalExecutable => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaunchTier0ProjectionError {
    selected_entry_count: usize,
}

impl LaunchTier0ProjectionError {
    pub fn selected_entry_count(self) -> usize {
        self.selected_entry_count
    }
}

#[derive(Debug)]
pub struct LaunchTier1Projection {
    entries: Vec<LaunchTier1ProjectionEntry>,
}

impl LaunchTier1Projection {
    pub fn into_entries(self) -> Vec<LaunchTier1ProjectionEntry> {
        self.entries
    }
}

#[derive(Debug)]
pub struct LaunchTier1ProjectionEntry {
    inventory_ordinal: usize,
    file: LaunchTier1AdmittedFile,
}

impl LaunchTier1ProjectionEntry {
    pub fn into_parts(self) -> (usize, LaunchTier1AdmittedFile) {
        (self.inventory_ordinal, self.file)
    }
}

#[derive(Debug)]
pub struct LaunchTier1AdmittedFile {
    root: KnownGoodRoot,
    physical_root: LaunchTier1PhysicalRoot,
    path: KnownGoodRelativePath,
    kind: KnownGoodArtifactKind,
    digest: Sha1Digest,
    size: u64,
}

impl LaunchTier1AdmittedFile {
    pub fn root(&self) -> &KnownGoodRoot {
        &self.root
    }

    #[cfg(test)]
    pub fn path(&self) -> &KnownGoodRelativePath {
        &self.path
    }

    pub fn kind(&self) -> KnownGoodArtifactKind {
        self.kind
    }

    pub fn digest(&self) -> &Sha1Digest {
        &self.digest
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn physical_path(&self, library_root: &Path) -> KnownGoodPhysicalPath {
        let mut relative = match self.physical_root {
            LaunchTier1PhysicalRoot::Versions => PathBuf::from("versions"),
            LaunchTier1PhysicalRoot::Libraries => PathBuf::from("libraries"),
        };
        for segment in self.path.as_str().split('/') {
            relative.push(segment);
        }
        KnownGoodPhysicalPath {
            root: library_root.to_path_buf(),
            relative,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchTier1PhysicalRoot {
    Versions,
    Libraries,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchTier1ProjectionError {
    TooManyEntries {
        selected_entry_count: usize,
    },
    UnsupportedIntegrity {
        selected_entry_count: usize,
        inventory_ordinal: usize,
    },
    ArtifactByteLimitExceeded {
        selected_entry_count: usize,
        inventory_ordinal: usize,
        expected_byte_count: u64,
    },
    AggregateByteLimitExceeded {
        selected_entry_count: usize,
        expected_byte_count: u64,
    },
}

impl LaunchTier1ProjectionError {
    pub fn selected_entry_count(self) -> usize {
        match self {
            Self::TooManyEntries {
                selected_entry_count,
            }
            | Self::UnsupportedIntegrity {
                selected_entry_count,
                ..
            }
            | Self::ArtifactByteLimitExceeded {
                selected_entry_count,
                ..
            }
            | Self::AggregateByteLimitExceeded {
                selected_entry_count,
                ..
            } => selected_entry_count,
        }
    }
}

#[derive(Debug)]
pub struct ManagedComponentProjection<'a> {
    component: ManagedKnownGoodComponent,
    entries: Vec<ManagedComponentProjectionEntry<'a>>,
    expected_content_byte_count: u64,
}

impl<'a> ManagedComponentProjection<'a> {
    pub fn component(&self) -> ManagedKnownGoodComponent {
        self.component
    }

    pub fn entries(&self) -> &[ManagedComponentProjectionEntry<'a>] {
        &self.entries
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn expected_content_byte_count(&self) -> u64 {
        self.expected_content_byte_count
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ManagedComponentProjectionEntry<'a> {
    inventory_ordinal: usize,
    entry: &'a KnownGoodEntry,
}

impl<'a> ManagedComponentProjectionEntry<'a> {
    pub fn inventory_ordinal(self) -> usize {
        self.inventory_ordinal
    }

    pub fn entry(self) -> &'a KnownGoodEntry {
        self.entry
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedComponentProjectionError {
    TooManyEntries {
        selected_entry_count: usize,
    },
    UnsupportedRootKind {
        selected_entry_count: usize,
        inventory_ordinal: usize,
    },
    UnsupportedIntegrity {
        selected_entry_count: usize,
        inventory_ordinal: usize,
    },
    ArtifactByteLimitExceeded {
        selected_entry_count: usize,
        inventory_ordinal: usize,
        expected_byte_count: u64,
    },
    AggregateByteCountOverflow {
        selected_entry_count: usize,
        inventory_ordinal: usize,
    },
    AggregateByteLimitExceeded {
        selected_entry_count: usize,
        expected_byte_count: u64,
    },
    PathCollision {
        selected_entry_count: usize,
        first_inventory_ordinal: usize,
        second_inventory_ordinal: usize,
    },
}

impl ManagedComponentProjectionError {
    pub fn selected_entry_count(self) -> usize {
        match self {
            Self::TooManyEntries {
                selected_entry_count,
            }
            | Self::UnsupportedRootKind {
                selected_entry_count,
                ..
            }
            | Self::UnsupportedIntegrity {
                selected_entry_count,
                ..
            }
            | Self::ArtifactByteLimitExceeded {
                selected_entry_count,
                ..
            }
            | Self::AggregateByteCountOverflow {
                selected_entry_count,
                ..
            }
            | Self::AggregateByteLimitExceeded {
                selected_entry_count,
                ..
            }
            | Self::PathCollision {
                selected_entry_count,
                ..
            } => selected_entry_count,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tier2Projection<'a> {
    entries: &'a [KnownGoodEntry],
    expected_content_byte_count: u64,
}

impl<'a> Tier2Projection<'a> {
    pub fn iter(self) -> Tier2ProjectionIter<'a> {
        Tier2ProjectionIter {
            entries: self.entries.iter().enumerate(),
        }
    }

    pub fn entry_count(self) -> usize {
        self.entries.len()
    }

    pub fn expected_content_byte_count(self) -> u64 {
        self.expected_content_byte_count
    }
}

pub struct Tier2ProjectionIter<'a> {
    entries: std::iter::Enumerate<std::slice::Iter<'a, KnownGoodEntry>>,
}

impl<'a> Iterator for Tier2ProjectionIter<'a> {
    type Item = Tier2ProjectionEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries
            .next()
            .map(|(inventory_ordinal, entry)| Tier2ProjectionEntry {
                inventory_ordinal,
                entry,
            })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl ExactSizeIterator for Tier2ProjectionIter<'_> {}
impl std::iter::FusedIterator for Tier2ProjectionIter<'_> {}

#[derive(Clone, Copy, Debug)]
pub struct Tier2ProjectionEntry<'a> {
    inventory_ordinal: usize,
    entry: &'a KnownGoodEntry,
}

impl<'a> Tier2ProjectionEntry<'a> {
    pub fn inventory_ordinal(self) -> usize {
        self.inventory_ordinal
    }

    pub fn entry(self) -> &'a KnownGoodEntry {
        self.entry
    }

    pub fn physical_path(
        self,
        library_root: &Path,
        runtime_cache: &crate::runtime::ManagedRuntimeCache,
    ) -> KnownGoodPhysicalPath {
        known_good_entry_path(library_root, runtime_cache, self.entry)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier2ProjectionError {
    TooManyEntries {
        entry_count: usize,
    },
    UnsupportedRootKind {
        entry_count: usize,
        inventory_ordinal: usize,
    },
    UnsupportedIntegrity {
        entry_count: usize,
        inventory_ordinal: usize,
    },
    ArtifactByteLimitExceeded {
        entry_count: usize,
        inventory_ordinal: usize,
        expected_byte_count: u64,
    },
    AggregateByteLimitExceeded {
        entry_count: usize,
        expected_byte_count: u64,
    },
}

impl Tier2ProjectionError {
    pub fn entry_count(self) -> usize {
        match self {
            Self::TooManyEntries { entry_count }
            | Self::UnsupportedRootKind { entry_count, .. }
            | Self::UnsupportedIntegrity { entry_count, .. }
            | Self::ArtifactByteLimitExceeded { entry_count, .. }
            | Self::AggregateByteLimitExceeded { entry_count, .. } => entry_count,
        }
    }
}

#[cfg(feature = "test-support")]
pub struct TestKnownGoodEntry {
    pub root: TestKnownGoodRoot,
    pub path: String,
    pub kind: KnownGoodArtifactKind,
    pub integrity: TestKnownGoodIntegrity,
}

#[cfg(feature = "test-support")]
pub enum TestKnownGoodRoot {
    Versions,
    Libraries,
    Assets,
    ManagedRuntime { component: String },
}

#[cfg(feature = "test-support")]
pub enum TestKnownGoodIntegrity {
    File { size: u64 },
    Sha1 { digest: String, size: u64 },
    ExactBytes { size: u64 },
    Directory,
    LinkTarget(String),
}

#[derive(Debug, Eq, PartialEq)]
pub struct KnownGoodInstallReceipt {
    authenticated: AuthenticatedKnownGoodReceipt,
}

#[derive(Eq, PartialEq)]
pub struct KnownGoodReconstructionReceipt {
    authenticated: AuthenticatedKnownGoodReceipt,
}

pub(crate) struct RetainedKnownGoodReconstruction {
    receipt: KnownGoodReconstructionReceipt,
    library_sources: RetainedLibrarySourceSet,
    version_bundle_sources: Option<RetainedVersionBundleReconstructionSources>,
}

pub(crate) struct ManagedVersionBundleReconstruction {
    projection: VersionBundleProjectionAuthority,
    managed_root: ManagedDir,
    source: AuthenticatedVersionBundleSource,
}

pub(crate) enum VersionBundleProjectionAuthority {
    Reconstructed(Box<KnownGoodReconstructionReceipt>),
    Registered {
        version_id: KnownGoodId,
        inventory: Arc<KnownGoodInventory>,
    },
}

pub(crate) struct ManagedLibrariesReconstruction {
    reconstruction: RetainedKnownGoodReconstruction,
    managed_root: ManagedDir,
    #[cfg(test)]
    library_entry_count: usize,
    #[cfg(test)]
    expected_content_byte_count: u64,
}

pub(crate) struct ManagedAssetsReconstruction {
    reconstruction: RetainedKnownGoodReconstruction,
    managed_root: ManagedDir,
    asset_sources: RetainedAssetSourceSet,
    #[cfg(test)]
    asset_entry_count: usize,
    #[cfg(test)]
    expected_content_byte_count: u64,
}

impl RetainedKnownGoodReconstruction {
    pub(crate) fn new(
        receipt: KnownGoodReconstructionReceipt,
        library_sources: RetainedLibrarySourceSet,
        version_bundle_sources: Option<RetainedVersionBundleReconstructionSources>,
    ) -> Self {
        Self {
            receipt,
            library_sources,
            version_bundle_sources,
        }
    }

    pub(crate) fn receipt(&self) -> &KnownGoodReconstructionReceipt {
        &self.receipt
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        KnownGoodReconstructionReceipt,
        RetainedLibrarySourceSet,
        Option<RetainedVersionBundleReconstructionSources>,
    ) {
        (
            self.receipt,
            self.library_sources,
            self.version_bundle_sources,
        )
    }

    pub(crate) fn discard_sources(self) -> KnownGoodReconstructionReceipt {
        self.receipt
    }

    #[cfg(test)]
    pub(crate) fn retained_version_bundle_sources_match_projection(&self) -> bool {
        let Ok(projection) = self
            .receipt
            .component_projection(ManagedKnownGoodComponent::VersionBundle)
        else {
            return false;
        };
        self.version_bundle_sources
            .as_ref()
            .is_some_and(|sources| sources.matches_projection(&projection))
    }

    pub(crate) fn bind_managed_version_bundle(
        self,
        managed_root: ManagedDir,
    ) -> Result<ManagedVersionBundleReconstruction, DownloadError> {
        let (receipt, _library_sources, version_bundle_sources) = self.into_parts();
        let projection = receipt
            .component_projection(ManagedKnownGoodComponent::VersionBundle)
            .map_err(|_| {
                DownloadError::Integrity(
                    "reconstructed VersionBundle projection is invalid".to_string(),
                )
            })?;
        let sources = version_bundle_sources.ok_or_else(|| {
            DownloadError::Integrity(
                "VersionBundle reconstruction did not retain exact final sources".to_string(),
            )
        })?;
        let source = AuthenticatedVersionBundleSource::from_reconstruction_projection(
            receipt.version_id().to_string(),
            &projection,
            sources,
        )?;
        Ok(ManagedVersionBundleReconstruction {
            projection: VersionBundleProjectionAuthority::Reconstructed(Box::new(receipt)),
            managed_root,
            source,
        })
    }

    pub(crate) fn bind_managed_libraries(
        mut self,
        managed_root: ManagedDir,
        cache_proofs: AuthenticatedLibraryCacheProofSet,
    ) -> Result<ManagedLibrariesReconstruction, DownloadError> {
        let projection = self
            .receipt
            .authenticated
            .inventory
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .map_err(|_| {
                DownloadError::Integrity(
                    "reconstructed Libraries projection is invalid".to_string(),
                )
            })?;
        self.library_sources
            .reconcile_sparse_projection(&projection, cache_proofs)?;
        Ok(ManagedLibrariesReconstruction {
            #[cfg(test)]
            library_entry_count: projection.entry_count(),
            #[cfg(test)]
            expected_content_byte_count: projection.expected_content_byte_count(),
            reconstruction: self,
            managed_root,
        })
    }

    pub(crate) fn bind_managed_assets(
        self,
        managed_root: ManagedDir,
        mut asset_sources: RetainedAssetSourceSet,
        cache_proofs: AuthenticatedAssetCacheProofSet,
    ) -> Result<ManagedAssetsReconstruction, DownloadError> {
        let projection = self
            .receipt
            .authenticated
            .inventory
            .managed_component_projection(ManagedKnownGoodComponent::Assets)
            .map_err(|_| {
                DownloadError::Integrity("reconstructed Assets projection is invalid".to_string())
            })?;
        asset_sources.reconcile_sparse_projection(&projection, cache_proofs)?;
        Ok(ManagedAssetsReconstruction {
            #[cfg(test)]
            asset_entry_count: projection.entry_count(),
            #[cfg(test)]
            expected_content_byte_count: projection.expected_content_byte_count(),
            reconstruction: self,
            managed_root,
            asset_sources,
        })
    }
}

impl ManagedVersionBundleReconstruction {
    pub(crate) fn from_registered(
        managed_root: ManagedDir,
        version_id: &str,
        inventory: Arc<KnownGoodInventory>,
        source: AuthenticatedVersionBundleSource,
    ) -> Result<Self, DownloadError> {
        let version_id = KnownGoodId::new(version_id).map_err(|_| {
            DownloadError::Integrity("registered VersionBundle identity is invalid".to_string())
        })?;
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .map_err(|_| {
                DownloadError::Integrity(
                    "registered VersionBundle projection is invalid".to_string(),
                )
            })?;
        if !source.matches_projection(&projection) || source.version_id() != version_id.as_str() {
            return Err(DownloadError::Integrity(
                "registered VersionBundle source does not match its projection".to_string(),
            ));
        }
        Ok(Self {
            projection: VersionBundleProjectionAuthority::Registered {
                version_id,
                inventory,
            },
            managed_root,
            source,
        })
    }

    pub(crate) fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        self.projection.matches_known_good_inventory(expected)
    }

    pub(crate) fn into_effect_parts(
        self,
    ) -> (
        ManagedDir,
        VersionBundleProjectionAuthority,
        AuthenticatedVersionBundleSource,
    ) {
        (self.managed_root, self.projection, self.source)
    }
}

impl VersionBundleProjectionAuthority {
    pub(crate) fn version_id(&self) -> &str {
        match self {
            Self::Reconstructed(receipt) => receipt.version_id(),
            Self::Registered { version_id, .. } => version_id.as_str(),
        }
    }

    pub(crate) fn component_projection(
        &self,
    ) -> Result<ManagedComponentProjection<'_>, ManagedComponentProjectionError> {
        match self {
            Self::Reconstructed(receipt) => {
                receipt.component_projection(ManagedKnownGoodComponent::VersionBundle)
            }
            Self::Registered { inventory, .. } => {
                inventory.managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            }
        }
    }

    pub(crate) fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        let Ok(ours) = self.component_projection() else {
            return false;
        };
        let Ok(expected) =
            expected.managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
        else {
            return false;
        };
        ours.entry_count() == expected.entry_count()
            && ours
                .entries()
                .iter()
                .zip(expected.entries())
                .all(|(left, right)| {
                    left.inventory_ordinal() == right.inventory_ordinal()
                        && left.entry() == right.entry()
                })
    }
}

impl ManagedLibrariesReconstruction {
    #[cfg(test)]
    pub(crate) fn version_id(&self) -> &str {
        self.reconstruction.receipt.version_id()
    }

    #[cfg(test)]
    pub(crate) fn library_entry_count(&self) -> usize {
        self.library_entry_count
    }

    #[cfg(test)]
    pub(crate) fn expected_content_byte_count(&self) -> u64 {
        self.expected_content_byte_count
    }

    #[cfg(test)]
    pub(crate) fn retained_source_count(&self) -> usize {
        self.reconstruction.library_sources.len()
    }

    #[cfg(test)]
    pub(crate) fn retained_content_byte_count(&self) -> u64 {
        self.reconstruction.library_sources.retained_bytes()
    }

    pub(crate) fn into_effect_parts(
        self,
    ) -> (
        ManagedDir,
        KnownGoodReconstructionReceipt,
        Vec<RetainedLibraryComponentSource>,
    ) {
        let (receipt, sources, _version_bundle_sources) = self.reconstruction.into_parts();
        (self.managed_root, receipt, sources.into_sources())
    }
}

impl ManagedAssetsReconstruction {
    #[cfg(test)]
    pub(crate) fn version_id(&self) -> &str {
        self.reconstruction.receipt.version_id()
    }

    #[cfg(test)]
    pub(crate) fn asset_entry_count(&self) -> usize {
        self.asset_entry_count
    }

    #[cfg(test)]
    pub(crate) fn expected_content_byte_count(&self) -> u64 {
        self.expected_content_byte_count
    }

    #[cfg(test)]
    pub(crate) fn retained_source_count(&self) -> usize {
        self.asset_sources.len()
    }

    pub(crate) fn into_effect_parts(
        self,
    ) -> (
        ManagedDir,
        KnownGoodReconstructionReceipt,
        Vec<RetainedAssetComponentSource>,
    ) {
        (
            self.managed_root,
            self.reconstruction.discard_sources(),
            self.asset_sources.into_sources(),
        )
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn managed_libraries_reconstruction_fixture_for_test(
    managed_root: ManagedDir,
    version_id: &str,
) -> Result<ManagedLibrariesReconstruction, DownloadError> {
    const PATH: &str = "org/axial/fixture/1.0.0/fixture-1.0.0.jar";
    const BYTES: &[u8] = b"axial managed Libraries fixture";
    let version_id = KnownGoodId::new(version_id).map_err(|_| {
        DownloadError::Integrity("managed Libraries fixture version id is invalid".to_string())
    })?;
    let path = PortableRelativePath::new(PATH).map_err(|_| {
        DownloadError::Integrity("managed Libraries fixture path is invalid".to_string())
    })?;
    let sha1: [u8; 20] = Sha1::digest(BYTES).into();
    let mut inventory = InventoryBuilder::default();
    inventory
        .insert(KnownGoodEntry {
            root: KnownGoodRoot::Libraries,
            path: KnownGoodRelativePath::new(PATH).map_err(|_| {
                DownloadError::Integrity(
                    "managed Libraries fixture inventory path is invalid".to_string(),
                )
            })?,
            kind: KnownGoodArtifactKind::Library,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: sha1_array_digest(&sha1),
                size: BYTES.len() as u64,
            },
        })
        .map_err(|_| {
            DownloadError::Integrity("managed Libraries fixture inventory is invalid".to_string())
        })?;
    let authenticated = AuthenticatedKnownGoodReceipt {
        version_id: version_id.clone(),
        inventory: inventory.finish(),
        effective_version: VersionJson {
            id: version_id.0,
            inherits_from: String::new(),
            materialized: false,
            kind: "release".to_string(),
            main_class: String::new(),
            minimum_launcher_version: 0,
            compliance_level: 0,
            release_time: String::new(),
            time: String::new(),
            arguments: None,
            minecraft_arguments: String::new(),
            asset_index: crate::launch::AssetIndex::default(),
            assets: String::new(),
            downloads: crate::launch::Downloads::default(),
            java_version: crate::launch::JavaVersion::default(),
            libraries: Vec::new(),
            logging: None,
        },
        environment: crate::rules::default_environment(),
    };
    let mut sources = RetainedLibrarySourceSet::new();
    sources.insert(
        RetainedLibraryComponentSource::from_authenticated_local_bytes(
            path,
            crate::download::library_source::LibraryComponentSourceKind::Library,
            BYTES.to_vec(),
            BYTES.len() as u64,
            sha1,
        )
        .map_err(|_| {
            DownloadError::Integrity("managed Libraries fixture source is invalid".to_string())
        })?,
    )?;
    Ok(ManagedLibrariesReconstruction {
        reconstruction: RetainedKnownGoodReconstruction::new(
            KnownGoodReconstructionReceipt { authenticated },
            sources,
            None,
        ),
        managed_root,
        #[cfg(test)]
        library_entry_count: 1,
        #[cfg(test)]
        expected_content_byte_count: BYTES.len() as u64,
    })
}

#[cfg(feature = "test-support")]
pub(crate) async fn managed_assets_reconstruction_fixture_for_test(
    managed_root: ManagedDir,
    version_id: &str,
) -> Result<ManagedAssetsReconstruction, DownloadError> {
    const INDEX_ID: &str = "fixture-assets";
    const OBJECT_BYTES: &[u8] = b"axial managed Assets fixture";
    let version_id = KnownGoodId::new(version_id).map_err(|_| {
        DownloadError::Integrity("managed Assets fixture version id is invalid".to_string())
    })?;
    let object_sha1: [u8; 20] = Sha1::digest(OBJECT_BYTES).into();
    let object_digest = sha1_array_digest(&object_sha1);
    let empty_sha1: [u8; 20] = Sha1::digest([]).into();
    let empty_digest = sha1_array_digest(&empty_sha1);
    let index_bytes = serde_json::to_vec(&serde_json::json!({
        "objects": {
            "fixture/object": {
                "hash": object_digest.as_str(),
                "size": OBJECT_BYTES.len()
            },
            "fixture/empty": {
                "hash": empty_digest.as_str(),
                "size": 0
            }
        }
    }))
    .map_err(|_| DownloadError::Integrity("managed Assets fixture index is invalid".to_string()))?;
    let index_sha1: [u8; 20] = Sha1::digest(&index_bytes).into();
    let index_path = format!("indexes/{INDEX_ID}.json");
    let object_path = format!(
        "objects/{}/{}",
        &object_digest.as_str()[..2],
        object_digest.as_str()
    );
    let empty_path = format!(
        "objects/{}/{}",
        &empty_digest.as_str()[..2],
        empty_digest.as_str()
    );
    let mut inventory = InventoryBuilder::default();
    for (path, kind, digest, size) in [
        (
            index_path.as_str(),
            KnownGoodArtifactKind::AssetIndex,
            index_sha1,
            index_bytes.len() as u64,
        ),
        (
            object_path.as_str(),
            KnownGoodArtifactKind::AssetObject,
            object_sha1,
            OBJECT_BYTES.len() as u64,
        ),
        (
            empty_path.as_str(),
            KnownGoodArtifactKind::AssetObject,
            empty_sha1,
            0,
        ),
    ] {
        inventory
            .insert(KnownGoodEntry {
                root: KnownGoodRoot::Assets,
                path: KnownGoodRelativePath::new(path).map_err(|_| {
                    DownloadError::Integrity(
                        "managed Assets fixture inventory path is invalid".to_string(),
                    )
                })?,
                kind,
                integrity: KnownGoodIntegrity::Sha1 {
                    digest: sha1_array_digest(&digest),
                    size,
                },
            })
            .map_err(|_| {
                DownloadError::Integrity("managed Assets fixture inventory is invalid".to_string())
            })?;
    }
    let authenticated = AuthenticatedKnownGoodReceipt {
        version_id: version_id.clone(),
        inventory: inventory.finish(),
        effective_version: VersionJson {
            id: version_id.0,
            inherits_from: String::new(),
            materialized: false,
            kind: "release".to_string(),
            main_class: String::new(),
            minimum_launcher_version: 0,
            compliance_level: 0,
            release_time: String::new(),
            time: String::new(),
            arguments: None,
            minecraft_arguments: String::new(),
            asset_index: crate::launch::AssetIndex {
                id: INDEX_ID.to_string(),
                sha1: format!("{:x}", Sha1::digest(&index_bytes)),
                size: index_bytes.len() as i64,
                total_size: OBJECT_BYTES.len() as i64,
                url: String::new(),
            },
            assets: INDEX_ID.to_string(),
            downloads: crate::launch::Downloads::default(),
            java_version: crate::launch::JavaVersion::default(),
            libraries: Vec::new(),
            logging: None,
        },
        environment: crate::rules::default_environment(),
    };
    let asset_workers = crate::managed_blocking::ManagedBlockingWorkers::new();
    let asset_attempt = asset_workers.attempt_guard();
    let source_pool = AssetSourcePool::new_with_workers(asset_workers.clone())?;
    let mut sources = RetainedAssetSourceSet::new();
    for (path, kind, bytes) in [
        (
            index_path.as_str(),
            ManagedComponentArtifactKind::AssetIndex,
            index_bytes,
        ),
        (
            object_path.as_str(),
            ManagedComponentArtifactKind::AssetObject,
            OBJECT_BYTES.to_vec(),
        ),
        (
            empty_path.as_str(),
            ManagedComponentArtifactKind::AssetObject,
            Vec::new(),
        ),
    ] {
        let path = PortableRelativePath::new(path).map_err(|_| {
            DownloadError::Integrity("managed Assets fixture source path is invalid".to_string())
        })?;
        sources.insert(
            source_pool
                .retain_authenticated_local_bytes(path, kind, bytes)
                .await?,
        )?;
    }
    asset_workers.drain().await;
    asset_attempt.disarm();
    RetainedKnownGoodReconstruction::new(
        KnownGoodReconstructionReceipt { authenticated },
        RetainedLibrarySourceSet::new(),
        None,
    )
    .bind_managed_assets(
        managed_root,
        sources,
        AuthenticatedAssetCacheProofSet::default(),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn managed_version_bundle_reconstruction_fixture_for_test(
    managed_root: ManagedDir,
    version_id: &str,
) -> Result<ManagedVersionBundleReconstruction, DownloadError> {
    const CLIENT_BYTES: &[u8] = b"axial managed VersionBundle client fixture";
    const LOG_ID: &str = "guardian-version-bundle.xml";
    const LOG_BYTES: &[u8] = b"<Configuration/>";
    let version_json = serde_json::to_vec(&serde_json::json!({
        "id": version_id,
        "type": "release",
        "mainClass": "org.axial.GuardianFixture"
    }))?;
    let version_id = KnownGoodId::new(version_id).map_err(|_| {
        DownloadError::Integrity("managed VersionBundle fixture id is invalid".to_string())
    })?;
    let inventory = KnownGoodInventory::version_bundle_for_test(
        version_id.as_str(),
        &version_json,
        CLIENT_BYTES,
        Some((LOG_ID, LOG_BYTES)),
    );
    let effective_version = serde_json::from_slice::<VersionJson>(&version_json)?;
    RetainedKnownGoodReconstruction::new(
        KnownGoodReconstructionReceipt {
            authenticated: AuthenticatedKnownGoodReceipt {
                version_id,
                inventory,
                effective_version,
                environment: crate::rules::default_environment(),
            },
        },
        RetainedLibrarySourceSet::new(),
        Some(
            RetainedVersionBundleReconstructionSources::from_local_final(
                version_json,
                CLIENT_BYTES.to_vec(),
                Some(LOG_BYTES.to_vec()),
            ),
        ),
    )
    .bind_managed_version_bundle(managed_root)
}

#[derive(Debug, Eq, PartialEq)]
pub struct KnownGoodActivationSource {
    version_id: KnownGoodId,
    inventory: KnownGoodInventory,
}

#[derive(Debug, Eq, PartialEq)]
struct AuthenticatedKnownGoodReceipt {
    version_id: KnownGoodId,
    inventory: KnownGoodInventory,
    effective_version: VersionJson,
    environment: Environment,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PendingKnownGoodInstallAuthority {
    authenticated: AuthenticatedKnownGoodReceipt,
}

pub(crate) struct PendingInstallerReceipt {
    authority: PendingKnownGoodInstallAuthority,
    library_sources: Vec<RetainedLibraryComponentSource>,
}

impl PendingInstallerReceipt {
    pub(crate) fn into_parts(
        self,
    ) -> (
        PendingKnownGoodInstallAuthority,
        Vec<RetainedLibraryComponentSource>,
    ) {
        (self.authority, self.library_sources)
    }
}

fn sha1_array_digest(value: &[u8; 20]) -> Sha1Digest {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(40);
    for byte in value {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    Sha1Digest(encoded)
}

impl KnownGoodInstallReceipt {
    pub fn version_id(&self) -> &str {
        self.authenticated.version_id.as_str()
    }

    pub fn into_activation_source(self) -> KnownGoodActivationSource {
        KnownGoodActivationSource {
            version_id: self.authenticated.version_id,
            inventory: self.authenticated.inventory,
        }
    }

    pub(crate) fn effective_version(&self) -> &VersionJson {
        &self.authenticated.effective_version
    }

    #[cfg(test)]
    pub(crate) fn from_test_authenticated_version(
        effective_version: VersionJson,
        environment: Environment,
    ) -> Self {
        let version_id = KnownGoodId::new(&effective_version.id).expect("safe test version id");
        let client = effective_version
            .downloads
            .client
            .as_ref()
            .expect("test authenticated version client");
        let mut inventory = InventoryBuilder::default();
        inventory
            .insert(KnownGoodEntry {
                root: KnownGoodRoot::Versions,
                path: KnownGoodRelativePath::new(&format!("{0}/{0}.jar", version_id.as_str()))
                    .expect("safe test client path"),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata(&client.sha1)
                        .expect("test authenticated client digest"),
                    size: u64::try_from(client.size).expect("test authenticated client size"),
                },
            })
            .expect("unique test client entry");
        if let Some(logging) = effective_version
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
        {
            inventory
                .insert(KnownGoodEntry {
                    root: KnownGoodRoot::Assets,
                    path: KnownGoodRelativePath::new(&format!("log_configs/{}", logging.file.id))
                        .expect("safe test log config path"),
                    kind: KnownGoodArtifactKind::LogConfig,
                    integrity: KnownGoodIntegrity::Sha1 {
                        digest: Sha1Digest::from_metadata(&logging.file.sha1)
                            .expect("test authenticated log config digest"),
                        size: u64::try_from(logging.file.size)
                            .expect("test authenticated log config size"),
                    },
                })
                .expect("unique test log config entry");
        }
        Self {
            authenticated: AuthenticatedKnownGoodReceipt {
                version_id,
                inventory: inventory.finish(),
                effective_version,
                environment,
            },
        }
    }

    pub(crate) fn authenticated_client_integrity(
        &self,
    ) -> Result<ExpectedIntegrity, KnownGoodInventoryError> {
        authenticated_client_expected(&self.authenticated)
    }

    pub(crate) fn authenticate_client_bytes(
        &self,
        bytes: &[u8],
    ) -> Result<(), KnownGoodInventoryError> {
        authenticate_client_bytes(&self.authenticated, bytes)
    }

    pub(crate) fn authenticate_log_config_bytes(
        &self,
        logical_identity: &str,
        bytes: &[u8],
    ) -> Result<(), KnownGoodInventoryError> {
        let expected_identity = self
            .authenticated
            .effective_version
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
            .map(|logging| logging.file.id.as_str())
            .ok_or(KnownGoodInventoryError::LogConfigIntegrity)?;
        if logical_identity != expected_identity {
            return Err(KnownGoodInventoryError::LogConfigIntegrity);
        }
        let expected_path = format!("log_configs/{logical_identity}");
        let entry = self
            .authenticated
            .inventory
            .entries()
            .iter()
            .find(|entry| {
                entry.root() == &KnownGoodRoot::Assets
                    && entry.kind() == KnownGoodArtifactKind::LogConfig
                    && entry.path().as_str() == expected_path
            })
            .ok_or(KnownGoodInventoryError::LogConfigIntegrity)?;
        let (digest, size) = match entry.integrity() {
            KnownGoodIntegrity::Sha1 { digest, size }
            | KnownGoodIntegrity::ExactBytes { digest, size } => (digest, *size),
            KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                return Err(KnownGoodInventoryError::LogConfigIntegrity);
            }
        };
        if u64::try_from(bytes.len()).ok() != Some(size) || sha1_digest(bytes) != *digest {
            return Err(KnownGoodInventoryError::LogConfigIntegrity);
        }
        Ok(())
    }

    pub(crate) fn from_verified_legacy_archive_source(
        base: &Self,
        record: &LoaderBuildRecord,
        resolved_version: VersionJson,
        version_bytes: &[u8],
        child_client_bytes: &[u8],
    ) -> Result<PendingKnownGoodInstallAuthority, KnownGoodInventoryError> {
        Ok(PendingKnownGoodInstallAuthority {
            authenticated: derive_legacy_archive_receipt(
                &base.authenticated,
                record,
                resolved_version,
                version_bytes,
                child_client_bytes,
            )?,
        })
    }

    pub(crate) fn from_verified_profile_source(
        base: &Self,
        record: &LoaderBuildRecord,
        resolved_version: VersionJson,
        version_bytes: &[u8],
        library_declarations: SealedExactLibraryDeclarations,
    ) -> Result<PendingKnownGoodInstallAuthority, KnownGoodInventoryError> {
        Ok(PendingKnownGoodInstallAuthority {
            authenticated: derive_profile_receipt(
                &base.authenticated,
                record,
                resolved_version,
                version_bytes,
                library_declarations,
            )?,
        })
    }

    pub(crate) fn from_verified_installer_source(
        base: Self,
        record: &LoaderBuildRecord,
        input: AuthenticatedInstallerReceiptInput,
        resolved_version: VersionJson,
        version_bytes: &[u8],
        base_client_bytes: &[u8],
        child_client: &VerifiedInstallerClientBytes,
    ) -> Result<PendingInstallerReceipt, KnownGoodInventoryError> {
        let (source, libraries) = input.into_parts();
        let (library_declarations, library_sources) = libraries.into_parts();
        let authenticated = derive_installer_receipt(
            base.authenticated,
            record,
            source,
            library_declarations,
            InstallerReceiptDerivation {
                resolved_version,
                version_bytes,
                base_client_bytes,
                child_client,
            },
        )?;
        Ok(PendingInstallerReceipt {
            authority: PendingKnownGoodInstallAuthority { authenticated },
            library_sources,
        })
    }
}

struct InstallerReceiptDerivation<'a> {
    resolved_version: VersionJson,
    version_bytes: &'a [u8],
    base_client_bytes: &'a [u8],
    child_client: &'a VerifiedInstallerClientBytes,
}

fn derive_installer_receipt(
    base: AuthenticatedKnownGoodReceipt,
    record: &LoaderBuildRecord,
    source: VerifiedInstallerReceiptSource,
    library_declarations: SealedExactLibraryDeclarations,
    derivation: InstallerReceiptDerivation<'_>,
) -> Result<AuthenticatedKnownGoodReceipt, KnownGoodInventoryError> {
    let InstallerReceiptDerivation {
        resolved_version,
        version_bytes,
        base_client_bytes,
        child_client,
    } = derivation;
    let strategy_matches = matches!(
        (record.component_id, record.strategy),
        (
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern | LoaderInstallStrategy::ForgeLegacyInstaller
        ) | (
            LoaderComponentId::NeoForge,
            LoaderInstallStrategy::NeoForgeModern
        )
    );
    let (installer_libraries, sealed_environment) = library_declarations
        .installer_contract()
        .ok_or(KnownGoodInventoryError::InstallerLibraryProofMismatch)?;
    if !strategy_matches
        || crate::loaders::api::validate_loader_build_record_identity(record).is_err()
        || base.version_id.as_str() != record.minecraft_version
        || base.effective_version.id != record.minecraft_version
        || resolved_version.id != record.version_id
        || source.source_bytes().is_empty()
        || sealed_environment != &base.environment
    {
        return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
    }
    authenticate_client_bytes(&base, base_client_bytes)?;
    if !child_client.matches_derivation(&source, base_client_bytes)
        || child_client.bytes().is_empty()
    {
        return Err(KnownGoodInventoryError::ClientIntegrity);
    }
    let child_client_bytes = child_client.bytes();
    let child_size = i64::try_from(child_client_bytes.len())
        .map_err(|_| KnownGoodInventoryError::InputTooLarge)?;
    let child_digest = sha1_digest(child_client_bytes);
    let mut recomposed = compose_loader_version(
        &base.effective_version,
        &record.minecraft_version,
        &record.version_id,
        source.version(),
    )
    .map_err(|_| KnownGoodInventoryError::LoaderIdentityMismatch)?;
    let recomposed_client = recomposed
        .downloads
        .client
        .as_mut()
        .ok_or(KnownGoodInventoryError::MissingClient)?;
    recomposed_client.sha1 = child_digest.as_str().to_string();
    recomposed_client.size = child_size;
    recomposed_client.url.clear();
    if recomposed != resolved_version
        || !serde_json::from_slice::<VersionJson>(version_bytes)
            .is_ok_and(|version| version == resolved_version)
    {
        return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
    }
    let child_client = resolved_version
        .downloads
        .client
        .as_ref()
        .ok_or(KnownGoodInventoryError::MissingClient)?;
    if u64::try_from(child_client.size).ok() != Some(child_client_bytes.len() as u64)
        || Sha1Digest::from_metadata(&child_client.sha1)? != child_digest
    {
        return Err(KnownGoodInventoryError::ClientIntegrity);
    }
    let version_id = KnownGoodId::new(&record.version_id)?;
    let mut builder = InventoryBuilder::default();
    add_inherited_assets_and_runtime(&mut builder, &base.inventory)?;
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
            size: child_client_bytes.len() as u64,
        },
    })?;

    let mut selected = BTreeMap::new();
    for plan in library_artifact_plans_for(&resolved_version.libraries, &base.environment)
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?
    {
        if selected
            .insert(plan.relative_path.clone(), (plan, true))
            .is_some()
        {
            return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
        }
    }
    for plan in library_artifact_plans_for(installer_libraries, sealed_environment)
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?
    {
        match selected.get(&plan.relative_path) {
            Some((existing, _)) if existing != &plan => {
                return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
            }
            Some(_) => {}
            None => {
                selected.insert(plan.relative_path.clone(), (plan, false));
            }
        }
    }
    let mut used_declarations = BTreeSet::new();
    for (_, (plan, allows_base)) in selected {
        let path = KnownGoodRelativePath::new(plan.relative_path.as_str())?;
        let kind = if plan.is_native {
            KnownGoodArtifactKind::NativeLibrary
        } else {
            KnownGoodArtifactKind::Library
        };
        let (integrity, provider_url) = if let Some((sealed_kind, sha1, size, provider_url)) =
            library_declarations.get(&plan.relative_path)
        {
            if sealed_kind
                != if plan.is_native {
                    SealedLibraryKind::Native
                } else {
                    SealedLibraryKind::Library
                }
                || plan.expected.size.is_some_and(|expected| size != expected)
                || plan.expected.sha1.as_deref().is_some_and(|expected_sha1| {
                    !Sha1Digest::from_metadata(expected_sha1)
                        .is_ok_and(|expected| expected == sha1_array_digest(&sha1))
                })
            {
                return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
            }
            used_declarations.insert(plan.relative_path.clone());
            (
                KnownGoodIntegrity::Sha1 {
                    digest: sha1_array_digest(&sha1),
                    size,
                },
                provider_url,
            )
        } else if allows_base {
            let entry = matching_base_library_entry(&base, &plan, &path, kind)
                .map_err(|_| KnownGoodInventoryError::InstallerLibraryProofMismatch)?;
            (
                entry.integrity().clone(),
                base.inventory
                    .inherited_repair_provider_for_entry(entry, plan.source_url.as_deref())
                    .map_err(|_| KnownGoodInventoryError::InstallerLibraryProofMismatch)?,
            )
        } else {
            return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
        };
        builder.insert_with_standalone_leaf_repair_source(
            KnownGoodEntry {
                root: KnownGoodRoot::Libraries,
                path,
                kind,
                integrity,
            },
            provider_url,
        )?;
    }
    if used_declarations.len() != library_declarations.len() {
        return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
    }
    Ok(AuthenticatedKnownGoodReceipt {
        version_id,
        inventory: builder.finish(),
        effective_version: resolved_version,
        environment: base.environment,
    })
}

fn authenticated_client_known_good_integrity(
    authenticated: &AuthenticatedKnownGoodReceipt,
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
    let path = format!("{0}/{0}.jar", authenticated.version_id.as_str());
    authenticated
        .inventory
        .entries
        .iter()
        .find(|entry| {
            entry.root == KnownGoodRoot::Versions
                && entry.path.as_str() == path
                && entry.kind == KnownGoodArtifactKind::ClientJar
        })
        .map(|entry| entry.integrity.clone())
        .ok_or(KnownGoodInventoryError::ClientIntegrity)
}

fn derive_legacy_archive_receipt(
    base: &AuthenticatedKnownGoodReceipt,
    record: &LoaderBuildRecord,
    resolved_version: VersionJson,
    version_bytes: &[u8],
    child_client_bytes: &[u8],
) -> Result<AuthenticatedKnownGoodReceipt, KnownGoodInventoryError> {
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
    let mut expected_version = base.effective_version.clone();
    expected_version.id = record.version_id.clone();
    expected_version.inherits_from = record.minecraft_version.clone();
    expected_version.materialized = true;
    let client = expected_version
        .downloads
        .client
        .as_mut()
        .ok_or(KnownGoodInventoryError::MissingClient)?;
    client.sha1 = child_digest.as_str().to_string();
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
    add_inherited_assets_and_runtime(&mut builder, &base.inventory)?;
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
            size: child_client_bytes.len() as u64,
        },
    })?;
    add_exact_inherited_libraries(
        &mut builder,
        &resolved_version.libraries,
        &base.environment,
        &base.inventory,
    )?;

    Ok(AuthenticatedKnownGoodReceipt {
        version_id,
        inventory: builder.finish(),
        effective_version: resolved_version,
        environment: base.environment.clone(),
    })
}

fn authenticate_client_bytes(
    authenticated: &AuthenticatedKnownGoodReceipt,
    bytes: &[u8],
) -> Result<(), KnownGoodInventoryError> {
    validate_bytes(bytes, &authenticated_client_expected(authenticated)?)
        .map_err(|_| KnownGoodInventoryError::ClientIntegrity)
}

fn authenticated_client_expected(
    authenticated: &AuthenticatedKnownGoodReceipt,
) -> Result<ExpectedIntegrity, KnownGoodInventoryError> {
    let client = authenticated
        .effective_version
        .downloads
        .client
        .as_ref()
        .ok_or(KnownGoodInventoryError::MissingClient)?;
    let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
    expected
        .sha1
        .as_deref()
        .ok_or(KnownGoodInventoryError::MissingChecksum)
        .and_then(Sha1Digest::from_metadata)?;
    Ok(expected)
}

fn derive_profile_receipt(
    base: &AuthenticatedKnownGoodReceipt,
    record: &LoaderBuildRecord,
    resolved_version: VersionJson,
    version_bytes: &[u8],
    library_declarations: SealedExactLibraryDeclarations,
) -> Result<AuthenticatedKnownGoodReceipt, KnownGoodInventoryError> {
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
    add_inherited_assets_and_runtime(&mut builder, &base.inventory)?;
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
        integrity: authenticated_client_known_good_integrity(base)?,
    })?;

    if resolved_version.libraries.len() > MAX_KNOWN_GOOD_ENTRIES {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    let (profile_fragment, sealed_environment) = library_declarations
        .profile_contract()
        .ok_or(KnownGoodInventoryError::ProfileLibraryProofMismatch)?;
    let recomposed = compose_loader_version(
        &base.effective_version,
        &record.minecraft_version,
        &record.version_id,
        profile_fragment,
    )
    .map_err(|_| KnownGoodInventoryError::ProfileLibraryProofMismatch)?;
    if sealed_environment != &base.environment || resolved_version != recomposed {
        return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
    }
    let libraries = library_artifact_plans_for(&resolved_version.libraries, &base.environment)
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    let mut used_proofs = BTreeSet::new();
    for plan in libraries {
        let path = KnownGoodRelativePath::new(plan.relative_path.as_str())?;
        let kind = if plan.is_native {
            KnownGoodArtifactKind::NativeLibrary
        } else {
            KnownGoodArtifactKind::Library
        };
        let (integrity, provider_url) =
            if let Some((sealed_kind, sealed_sha1, sealed_size, provider_url)) =
                library_declarations.get(&plan.relative_path)
            {
                let expected_kind = if plan.is_native {
                    SealedLibraryKind::Native
                } else {
                    SealedLibraryKind::Library
                };
                if sealed_kind != expected_kind
                    || plan.expected.size != Some(sealed_size)
                    || plan.expected.sha1.as_deref().is_none_or(|sha1| {
                        !Sha1Digest::from_metadata(sha1)
                            .is_ok_and(|digest| digest == sha1_array_digest(&sealed_sha1))
                    })
                {
                    return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
                }
                used_proofs.insert(plan.relative_path.clone());
                (
                    KnownGoodIntegrity::Sha1 {
                        digest: sha1_array_digest(&sealed_sha1),
                        size: sealed_size,
                    },
                    provider_url,
                )
            } else {
                let entry = matching_base_library_entry(base, &plan, &path, kind)?;
                (
                    entry.integrity().clone(),
                    base.inventory
                        .inherited_repair_provider_for_entry(entry, plan.source_url.as_deref())?,
                )
            };
        builder.insert_with_standalone_leaf_repair_source(
            KnownGoodEntry {
                root: KnownGoodRoot::Libraries,
                path,
                kind,
                integrity,
            },
            provider_url,
        )?;
    }
    if used_proofs.len() != library_declarations.len() {
        return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
    }

    Ok(AuthenticatedKnownGoodReceipt {
        version_id,
        inventory: builder.finish(),
        effective_version: resolved_version,
        environment: base.environment.clone(),
    })
}

impl KnownGoodReconstructionReceipt {
    pub fn version_id(&self) -> &str {
        self.authenticated.version_id.as_str()
    }

    pub fn into_activation_source(self) -> KnownGoodActivationSource {
        KnownGoodActivationSource {
            version_id: self.authenticated.version_id,
            inventory: self.authenticated.inventory,
        }
    }

    pub(crate) fn component_projection(
        &self,
        component: ManagedKnownGoodComponent,
    ) -> Result<ManagedComponentProjection<'_>, ManagedComponentProjectionError> {
        self.authenticated
            .inventory
            .managed_component_projection(component)
    }
}

pub(crate) fn reconstructed_effective_version(
    receipt: &KnownGoodReconstructionReceipt,
) -> &VersionJson {
    &receipt.authenticated.effective_version
}

pub(crate) fn seal_reconstructed_profile_source(
    base: RetainedKnownGoodReconstruction,
    record: &LoaderBuildRecord,
    resolved_version: VersionJson,
    version_bytes: Vec<u8>,
    library_declarations: SealedExactLibraryDeclarations,
    library_sources: RetainedLibrarySourceSet,
) -> Result<RetainedKnownGoodReconstruction, KnownGoodInventoryError> {
    let (base, mut retained_sources, version_bundle_sources) = base.into_parts();
    retained_sources
        .merge(library_sources)
        .map_err(|_| KnownGoodInventoryError::ProfileLibraryProofMismatch)?;
    let authenticated = derive_profile_receipt(
        &base.authenticated,
        record,
        resolved_version,
        &version_bytes,
        library_declarations,
    )?;
    Ok(RetainedKnownGoodReconstruction::new(
        KnownGoodReconstructionReceipt { authenticated },
        retained_sources,
        version_bundle_sources.map(|sources| sources.replace_final(version_bytes, None)),
    ))
}

pub(crate) fn seal_reconstructed_installer_source(
    authority: AuthenticatedInstallerReconstructionAuthority,
) -> Result<RetainedKnownGoodReconstruction, KnownGoodInventoryError> {
    let (
        base,
        base_client_source,
        record,
        input,
        resolved_version,
        version_bytes,
        child_client,
        library_sources,
    ) = authority.consume_for_sealing();
    let (base, mut retained_sources, version_bundle_sources) = base.into_parts();
    let (source, library_declarations, local_library_sources) = input.into_parts();
    if !local_library_sources.is_empty() {
        return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
    }
    retained_sources
        .merge(library_sources)
        .map_err(|_| KnownGoodInventoryError::InstallerLibraryProofMismatch)?;
    if !source.matches_record(&record) {
        return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
    }
    authenticate_reconstructed_client_source(&base, &base_client_source)?;
    let authenticated = derive_installer_receipt(
        base.authenticated,
        &record,
        source,
        library_declarations,
        InstallerReceiptDerivation {
            resolved_version,
            version_bytes: &version_bytes,
            base_client_bytes: base_client_source.bytes(),
            child_client: &child_client,
        },
    )?;
    let child_client = child_client.into_bytes();
    Ok(RetainedKnownGoodReconstruction::new(
        KnownGoodReconstructionReceipt { authenticated },
        retained_sources,
        version_bundle_sources
            .map(|sources| sources.replace_final(version_bytes, Some(child_client))),
    ))
}

pub(crate) fn seal_reconstructed_legacy_archive_source(
    authority: AuthenticatedLegacyOverlayAuthority,
) -> Result<RetainedKnownGoodReconstruction, KnownGoodInventoryError> {
    let (
        base,
        base_client_source,
        archive_source,
        record,
        resolved_version,
        version_bytes,
        child_client_bytes,
    ) = authority.consume_for_sealing();
    let (base, library_sources, version_bundle_sources) = base.into_parts();
    let LoaderInstallSource::LegacyArchive { url: archive_url } = &record.install_source else {
        return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
    };
    if !archive_source.matches_contract(archive_url, &record.version_id) {
        return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
    }
    authenticate_reconstructed_client_source(&base, &base_client_source)?;
    let authenticated = derive_legacy_archive_receipt(
        &base.authenticated,
        &record,
        resolved_version,
        &version_bytes,
        &child_client_bytes,
    )?;
    Ok(RetainedKnownGoodReconstruction::new(
        KnownGoodReconstructionReceipt { authenticated },
        library_sources,
        version_bundle_sources
            .map(|sources| sources.replace_final(version_bytes, Some(child_client_bytes))),
    ))
}

fn authenticate_reconstructed_client_source(
    base: &KnownGoodReconstructionReceipt,
    source: &AuthenticatedSelectedArtifactSource,
) -> Result<(), KnownGoodInventoryError> {
    let client = base
        .authenticated
        .effective_version
        .downloads
        .client
        .as_ref()
        .ok_or(KnownGoodInventoryError::MissingClient)?;
    let expected = ExpectedIntegrity {
        size: u64::try_from(client.size).ok().filter(|size| *size > 0),
        sha1: (!client.sha1.trim().is_empty()).then(|| client.sha1.trim().to_string()),
    };
    if source.kind() != SelectedDownloadArtifactKind::ClientJar
        || source.logical_identity() != base.authenticated.version_id.as_str()
        || source.provider_url() != client.url
        || source.expected() != &expected
        || validate_bytes(source.bytes(), &expected).is_err()
    {
        return Err(KnownGoodInventoryError::ClientIntegrity);
    }
    Ok(())
}

pub(crate) fn seal_reconstructed_vanilla(
    authority: ReconstructedVanillaAuthority,
) -> Result<RetainedKnownGoodReconstruction, KnownGoodInventoryError> {
    let ReconstructedVanillaAuthorityParts {
        version,
        environment,
        libraries,
        version_source,
        asset_source,
        runtime_source,
        library_sources,
        version_bundle_sources,
    } = authority.into_parts();
    let authenticated = authenticate_vanilla_authority(
        &version,
        &environment,
        &libraries,
        &version_source,
        asset_source.as_ref(),
        runtime_source.as_ref(),
    )?;
    Ok(RetainedKnownGoodReconstruction::new(
        KnownGoodReconstructionReceipt { authenticated },
        library_sources,
        version_bundle_sources,
    ))
}

pub(crate) fn authenticate_pending_known_good_install(
    authority: &AuthenticatedVanillaInstallSources,
) -> Result<PendingKnownGoodInstallAuthority, KnownGoodInventoryError> {
    let (version, environment, libraries, version_source, asset_source, runtime_source) =
        authority.authentication_parts();
    let authenticated = authenticate_vanilla_authority(
        version,
        environment,
        libraries,
        version_source,
        asset_source,
        runtime_source,
    )?;
    Ok(PendingKnownGoodInstallAuthority { authenticated })
}

fn authenticate_vanilla_authority(
    resolved_version: &VersionJson,
    environment: &Environment,
    libraries: &SealedExactLibraryDeclarations,
    version_source: &AuthenticatedSelectedArtifactSource,
    asset_source: Option<&AuthenticatedSelectedArtifactSource>,
    runtime_source: Option<&RuntimeSourceReceipt>,
) -> Result<AuthenticatedKnownGoodReceipt, KnownGoodInventoryError> {
    if version_source.kind() != SelectedDownloadArtifactKind::VersionJson
        || version_source.logical_identity() != resolved_version.id.as_str()
        || version_source.provider_url().trim().is_empty()
    {
        return Err(KnownGoodInventoryError::VersionIdentityMismatch);
    }
    validate_bytes(version_source.bytes(), version_source.expected())
        .map_err(|_| KnownGoodInventoryError::VersionMetadataIntegrity)?;
    let mut authenticated = serde_json::from_slice::<VersionJson>(version_source.bytes())
        .map_err(|_| KnownGoodInventoryError::VersionMetadataIntegrity)?;
    if authenticated.asset_index.id.is_empty() && !authenticated.assets.is_empty() {
        authenticated
            .asset_index
            .id
            .clone_from(&authenticated.assets);
    }
    authenticated.java_version = effective_java_version_for(
        &authenticated.id,
        &authenticated.kind,
        &authenticated.java_version,
    );
    if &authenticated != resolved_version {
        return Err(KnownGoodInventoryError::VersionIdentityMismatch);
    }
    let asset_index_bytes = match asset_source {
        Some(source)
            if source.kind() == SelectedDownloadArtifactKind::AssetIndex
                && source.logical_identity() == resolved_version.asset_index.id.as_str()
                && source.provider_url() == resolved_version.asset_index.url.as_str()
                && source.expected()
                    == &ExpectedIntegrity::from_mojang(
                        resolved_version.asset_index.size,
                        &resolved_version.asset_index.sha1,
                    ) =>
        {
            Some(source.bytes())
        }
        Some(_) => return Err(KnownGoodInventoryError::AssetIndexIntegrity),
        None => None,
    };
    let version_id = KnownGoodId::new(&resolved_version.id)?;
    let version_metadata_size = u64::try_from(version_source.bytes().len())
        .map_err(|_| KnownGoodInventoryError::InputTooLarge)?;
    let runtime_observation = runtime_source.map(|receipt| RuntimeSourceObservation {
        component: receipt.component(),
        manifest_bytes: receipt.bytes(),
        manifest_expected: ExpectedIntegrity {
            size: Some(receipt.expected_size()),
            sha1: Some(receipt.expected_sha1().to_string()),
        },
    });
    let inventory = derive_known_good_inventory(
        resolved_version,
        libraries,
        asset_index_bytes,
        VersionSourceObservation {
            identity: version_source.logical_identity(),
            expected: version_source.expected(),
            metadata_size: version_metadata_size,
        },
        runtime_observation,
        environment,
    )?;
    Ok(AuthenticatedKnownGoodReceipt {
        version_id,
        inventory,
        effective_version: resolved_version.clone(),
        environment: environment.clone(),
    })
}

impl PendingKnownGoodInstallAuthority {
    #[cfg(test)]
    pub(crate) fn component_for_test(
        entries: impl IntoIterator<Item = (KnownGoodRoot, String, KnownGoodArtifactKind, [u8; 20], u64)>,
    ) -> Self {
        let mut inventory = InventoryBuilder::default();
        for (root, path, kind, digest, size) in entries {
            inventory
                .insert(KnownGoodEntry {
                    root,
                    path: KnownGoodRelativePath::new(&path).expect("test component path"),
                    kind,
                    integrity: KnownGoodIntegrity::Sha1 {
                        digest: sha1_array_digest(&digest),
                        size,
                    },
                })
                .expect("unique test component entry");
        }
        let version_id = KnownGoodId::new("test-component-intent").expect("test version id");
        Self {
            authenticated: AuthenticatedKnownGoodReceipt {
                version_id: version_id.clone(),
                inventory: inventory.finish(),
                effective_version: VersionJson {
                    id: version_id.0,
                    inherits_from: String::new(),
                    materialized: false,
                    kind: "release".to_string(),
                    main_class: String::new(),
                    minimum_launcher_version: 0,
                    compliance_level: 0,
                    release_time: String::new(),
                    time: String::new(),
                    arguments: None,
                    minecraft_arguments: String::new(),
                    asset_index: crate::launch::AssetIndex::default(),
                    assets: String::new(),
                    downloads: crate::launch::Downloads::default(),
                    java_version: crate::launch::JavaVersion::default(),
                    libraries: Vec::new(),
                    logging: None,
                },
                environment: crate::rules::default_environment(),
            },
        }
    }

    pub(crate) fn version_id(&self) -> &str {
        self.authenticated.version_id.as_str()
    }

    pub(crate) fn version_bundle_projection(
        &self,
    ) -> Result<ManagedComponentProjection<'_>, ManagedComponentProjectionError> {
        self.authenticated
            .inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
    }

    pub(crate) fn component_projection(
        &self,
        component: ManagedKnownGoodComponent,
    ) -> Result<ManagedComponentProjection<'_>, ManagedComponentProjectionError> {
        self.authenticated
            .inventory
            .managed_component_projection(component)
    }

    pub(crate) fn seal_after_version_bundle_commit(self) -> KnownGoodInstallReceipt {
        KnownGoodInstallReceipt {
            authenticated: self.authenticated,
        }
    }
}

impl KnownGoodActivationSource {
    pub fn into_parts(self) -> (String, KnownGoodInventory) {
        (self.version_id.0, self.inventory)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KnownGoodRelativePath(String);

impl KnownGoodRelativePath {
    pub fn new(value: &str) -> Result<Self, KnownGoodInventoryError> {
        PortableRelativePath::new(value)
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

    pub(crate) fn to_bytes(&self) -> [u8; 20] {
        let mut decoded = [0_u8; 20];
        for (index, pair) in self.0.as_bytes().chunks_exact(2).enumerate() {
            decoded[index] = (sha1_hex_nibble(pair[0]) << 4) | sha1_hex_nibble(pair[1]);
        }
        decoded
    }
}

fn sha1_hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("Sha1Digest stores canonical lowercase hexadecimal"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownGoodLinkTarget(String);

impl KnownGoodLinkTarget {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KnownGoodInventoryError {
    UnsafePath,
    InvalidSha1,
    MissingChecksum,
    MissingSize,
    SizeMismatch,
    MissingAssetIndex,
    UnexpectedAssetIndex,
    AssetIndexIntegrity,
    VersionMetadataIntegrity,
    ClientIntegrity,
    LogConfigIntegrity,
    AssetIndexParse,
    InvalidAssetObject,
    UnsupportedRuntimeEntry,
    MissingRuntimeDownload,
    MissingRuntimeExecutable,
    RuntimeManifestParse,
    RuntimeManifestIntegrity,
    VersionIdentityMismatch,
    LoaderIdentityMismatch,
    RuntimeIdentityMismatch,
    MetadataSerialization,
    MissingClient,
    InputTooLarge,
    InvalidLibraryPlan,
    VanillaLibraryProofMismatch,
    ProfileLibraryProofMismatch,
    InstallerLibraryProofMismatch,
    ConflictingEntry,
    InvalidRepairSource,
    ConflictingRepairSource,
    ConflictingRuntimePath,
    TooManyEntries,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnownGoodRepairSourceError {
    UnknownInventoryOrdinal,
    UnsupportedInventoryEntry,
    ContractMismatch,
}

struct VersionSourceObservation<'a> {
    identity: &'a str,
    expected: &'a ExpectedIntegrity,
    metadata_size: u64,
}

struct RuntimeSourceObservation<'a> {
    component: &'a RuntimeId,
    manifest_bytes: &'a [u8],
    manifest_expected: ExpectedIntegrity,
}

fn derive_known_good_inventory(
    resolved_version: &VersionJson,
    libraries: &SealedExactLibraryDeclarations,
    asset_index_bytes: Option<&[u8]>,
    version_source: VersionSourceObservation<'_>,
    runtime_source: Option<RuntimeSourceObservation<'_>>,
    environment: &Environment,
) -> Result<KnownGoodInventory, KnownGoodInventoryError> {
    let version_id = KnownGoodId::new(&resolved_version.id)?;
    let mut builder = InventoryBuilder::default();
    let version_base = version_id.as_str();

    builder.insert(KnownGoodEntry {
        root: KnownGoodRoot::Versions,
        path: KnownGoodRelativePath::new(&format!("{version_base}/{version_base}.json"))?,
        kind: KnownGoodArtifactKind::VersionMetadata,
        integrity: version_metadata_integrity(
            version_source.identity,
            version_source.expected,
            resolved_version,
            version_source.metadata_size,
        )?,
    })?;

    let client = resolved_version
        .downloads
        .client
        .as_ref()
        .ok_or(KnownGoodInventoryError::MissingClient)?;
    builder.insert(KnownGoodEntry {
        root: KnownGoodRoot::Versions,
        path: KnownGoodRelativePath::new(&format!("{version_base}/{version_base}.jar"))?,
        kind: KnownGoodArtifactKind::ClientJar,
        integrity: expected_integrity_with_observed_size(
            &ExpectedIntegrity::from_mojang(client.size, &client.sha1),
            authenticated_mojang_size(client.size)?,
        )?,
    })?;

    if resolved_version.libraries.len() > MAX_KNOWN_GOOD_ENTRIES {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    add_sealed_libraries(&mut builder, resolved_version, environment, libraries)?;

    if let Some(logging) = resolved_version
        .logging
        .as_ref()
        .and_then(|logging| logging.client.as_ref())
    {
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Assets,
            path: KnownGoodRelativePath::new(&format!("log_configs/{}", logging.file.id))?,
            kind: KnownGoodArtifactKind::LogConfig,
            integrity: expected_integrity_with_observed_size(
                &ExpectedIntegrity::from_mojang(logging.file.size, &logging.file.sha1),
                authenticated_mojang_size(logging.file.size)?,
            )?,
        })?;
    }

    add_asset_index(&mut builder, resolved_version, asset_index_bytes)?;
    if let Some(runtime_source) = runtime_source {
        if runtime_source.component.as_str()
            != preferred_runtime_component(&resolved_version.java_version)
        {
            return Err(KnownGoodInventoryError::RuntimeIdentityMismatch);
        }
        add_runtime(
            &mut builder,
            runtime_source.component,
            runtime_source.manifest_bytes,
            &runtime_source.manifest_expected,
        )?;
    }
    Ok(builder.finish())
}

fn version_metadata_integrity(
    source_identity: &str,
    source_expected: &ExpectedIntegrity,
    version: &VersionJson,
    observed_size: u64,
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
    if source_identity != version.id {
        return Err(KnownGoodInventoryError::VersionIdentityMismatch);
    }
    expected_integrity_with_observed_size(source_expected, observed_size)
}

fn add_sealed_libraries(
    builder: &mut InventoryBuilder,
    version: &VersionJson,
    environment: &Environment,
    authority: &SealedExactLibraryDeclarations,
) -> Result<(), KnownGoodInventoryError> {
    let plans = library_artifact_plans_for(&version.libraries, environment)
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    if !authority.matches_version(version, environment) {
        return Err(KnownGoodInventoryError::VanillaLibraryProofMismatch);
    }
    if plans.len() != authority.len() {
        return Err(KnownGoodInventoryError::VanillaLibraryProofMismatch);
    }
    for plan in plans {
        let (kind, observed_sha1, size, provider_url) = authority
            .get(&plan.relative_path)
            .ok_or(KnownGoodInventoryError::VanillaLibraryProofMismatch)?;
        let expected_kind = if plan.is_native {
            SealedLibraryKind::Native
        } else {
            SealedLibraryKind::Library
        };
        if kind != expected_kind
            || plan.expected.size.is_some_and(|expected| expected != size)
            || plan.expected.sha1.as_deref().is_some_and(|expected_sha1| {
                !Sha1Digest::from_metadata(expected_sha1)
                    .is_ok_and(|digest| digest == sha1_array_digest(&observed_sha1))
            })
        {
            return Err(KnownGoodInventoryError::VanillaLibraryProofMismatch);
        }
        builder.insert_with_standalone_leaf_repair_source(
            KnownGoodEntry {
                root: KnownGoodRoot::Libraries,
                path: KnownGoodRelativePath::new(plan.relative_path.as_str())?,
                kind: if plan.is_native {
                    KnownGoodArtifactKind::NativeLibrary
                } else {
                    KnownGoodArtifactKind::Library
                },
                integrity: KnownGoodIntegrity::Sha1 {
                    digest: sha1_array_digest(&observed_sha1),
                    size,
                },
            },
            provider_url,
        )?;
    }
    Ok(())
}

fn add_exact_inherited_libraries(
    builder: &mut InventoryBuilder,
    libraries: &[Library],
    environment: &Environment,
    base: &KnownGoodInventory,
) -> Result<(), KnownGoodInventoryError> {
    let plans = library_artifact_plans_for(libraries, environment)
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    for plan in plans {
        let path = KnownGoodRelativePath::new(plan.relative_path.as_str())?;
        let kind = if plan.is_native {
            KnownGoodArtifactKind::NativeLibrary
        } else {
            KnownGoodArtifactKind::Library
        };
        let entry = base
            .entries
            .iter()
            .find(|entry| {
                entry.root == KnownGoodRoot::Libraries && entry.path == path && entry.kind == kind
            })
            .ok_or(KnownGoodInventoryError::VanillaLibraryProofMismatch)?;
        let KnownGoodIntegrity::Sha1 { digest, size } = &entry.integrity else {
            return Err(KnownGoodInventoryError::VanillaLibraryProofMismatch);
        };
        if plan.expected.size.is_some_and(|expected| expected != *size)
            || plan.expected.sha1.as_deref().is_some_and(|expected| {
                !Sha1Digest::from_metadata(expected).is_ok_and(|expected| expected == *digest)
            })
        {
            return Err(KnownGoodInventoryError::VanillaLibraryProofMismatch);
        }
        let provider_url =
            base.inherited_repair_provider_for_entry(entry, plan.source_url.as_deref())?;
        builder.insert_with_standalone_leaf_repair_source(entry.clone(), provider_url)?;
    }
    Ok(())
}

fn add_inherited_assets_and_runtime(
    builder: &mut InventoryBuilder,
    base: &KnownGoodInventory,
) -> Result<(), KnownGoodInventoryError> {
    for (inventory_ordinal, entry) in base.entries.iter().enumerate().filter(|(_, entry)| {
        matches!(
            entry.root(),
            KnownGoodRoot::Assets | KnownGoodRoot::ManagedRuntime { .. }
        )
    }) {
        builder.insert_preserving_standalone_leaf_repair_source(
            base,
            inventory_ordinal,
            entry.clone(),
        )?;
    }
    Ok(())
}

fn matching_base_library_entry<'a>(
    base: &'a AuthenticatedKnownGoodReceipt,
    plan: &LibraryArtifactPlan,
    path: &KnownGoodRelativePath,
    kind: KnownGoodArtifactKind,
) -> Result<&'a KnownGoodEntry, KnownGoodInventoryError> {
    let expected_digest = plan
        .expected
        .sha1
        .as_deref()
        .ok_or(KnownGoodInventoryError::MissingChecksum)
        .and_then(Sha1Digest::from_metadata)?;
    let entry = base
        .inventory
        .entries
        .iter()
        .find(|entry| {
            entry.root == KnownGoodRoot::Libraries && entry.path == *path && entry.kind == kind
        })
        .ok_or(KnownGoodInventoryError::VanillaLibraryProofMismatch)?;
    let KnownGoodIntegrity::Sha1 { digest, size } = &entry.integrity else {
        return Err(KnownGoodInventoryError::VanillaLibraryProofMismatch);
    };
    if plan.expected.size.is_some_and(|expected| expected != *size) || expected_digest != *digest {
        return Err(KnownGoodInventoryError::VanillaLibraryProofMismatch);
    }
    Ok(entry)
}

fn add_asset_index(
    builder: &mut InventoryBuilder,
    version: &VersionJson,
    bytes: Option<&[u8]>,
) -> Result<(), KnownGoodInventoryError> {
    if bytes.is_some_and(|bytes| bytes.len() > MAX_KNOWN_GOOD_ASSET_INDEX_BYTES) {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    let asset_index = &version.asset_index;
    let absent = asset_index.id.is_empty()
        && asset_index.url.is_empty()
        && asset_index.sha1.is_empty()
        && asset_index.size == 0
        && asset_index.total_size == 0;
    if absent {
        return if bytes.is_none() {
            Ok(())
        } else {
            Err(KnownGoodInventoryError::UnexpectedAssetIndex)
        };
    }
    if asset_index.id.trim().is_empty()
        || asset_index.url.trim().is_empty()
        || asset_index.size < 0
        || asset_index.total_size < 0
    {
        return Err(KnownGoodInventoryError::AssetIndexIntegrity);
    }
    let bytes = bytes.ok_or(KnownGoodInventoryError::MissingAssetIndex)?;
    let index_id = KnownGoodId::new(&asset_index.id)?;
    let expected = ExpectedIntegrity::from_mojang(asset_index.size, &asset_index.sha1);
    validate_bytes(bytes, &expected).map_err(|_| KnownGoodInventoryError::AssetIndexIntegrity)?;
    builder.insert_with_standalone_leaf_repair_source(
        KnownGoodEntry {
            root: KnownGoodRoot::Assets,
            path: KnownGoodRelativePath::new(&format!("indexes/{}.json", index_id.as_str()))?,
            kind: KnownGoodArtifactKind::AssetIndex,
            integrity: expected_integrity_with_observed_size(&expected, bytes.len() as u64)?,
        },
        Some(&asset_index.url),
    )?;

    let index = parse_asset_index(bytes).map_err(|_| KnownGoodInventoryError::AssetIndexParse)?;
    for object in index.objects.values() {
        let digest = Sha1Digest::from_metadata(&object.hash)
            .map_err(|_| KnownGoodInventoryError::InvalidAssetObject)?;
        let size =
            u64::try_from(object.size).map_err(|_| KnownGoodInventoryError::InvalidAssetObject)?;
        let provider_url = asset_object_provider_url(&digest);
        builder.insert_with_standalone_leaf_repair_source(
            KnownGoodEntry {
                root: KnownGoodRoot::Assets,
                path: KnownGoodRelativePath::new(&asset_object_path(&digest))?,
                kind: KnownGoodArtifactKind::AssetObject,
                integrity: KnownGoodIntegrity::Sha1 { digest, size },
            },
            Some(&provider_url),
        )?;
    }
    Ok(())
}

fn add_runtime(
    builder: &mut InventoryBuilder,
    component: &RuntimeId,
    manifest_bytes: &[u8],
    manifest_expected: &ExpectedIntegrity,
) -> Result<(), KnownGoodInventoryError> {
    if manifest_bytes.len() > MAX_KNOWN_GOOD_RUNTIME_MANIFEST_BYTES {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    validate_bytes(manifest_bytes, manifest_expected)
        .map_err(|_| KnownGoodInventoryError::RuntimeManifestIntegrity)?;
    let manifest = serde_json::from_slice::<ComponentManifest>(manifest_bytes)
        .map_err(|_| KnownGoodInventoryError::RuntimeManifestParse)?;
    let manifest_proof = component_manifest_proof_bytes(&manifest)
        .map_err(|_| KnownGoodInventoryError::MetadataSerialization)?;
    let root = KnownGoodRoot::ManagedRuntime {
        component: KnownGoodId::new(component.as_str())?,
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
    let java_path = crate::runtime::runtime_java_relative_path();
    let mut saw_java = false;
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
            kind: if path == java_path {
                saw_java = true;
                KnownGoodArtifactKind::RuntimeExecutable
            } else {
                KnownGoodArtifactKind::RuntimeFile
            },
            integrity: expected_integrity(&expected)?,
        });
    }
    for (path, file) in plan.link_entries {
        let target = file
            .target
            .ok_or(KnownGoodInventoryError::UnsupportedRuntimeEntry)?;
        if cfg!(target_os = "windows") && path == java_path {
            return Err(KnownGoodInventoryError::UnsupportedRuntimeEntry);
        }
        entries.push(KnownGoodEntry {
            root: root.clone(),
            path: KnownGoodRelativePath::new(&path)?,
            kind: if path == java_path {
                saw_java = true;
                KnownGoodArtifactKind::RuntimeExecutable
            } else {
                KnownGoodArtifactKind::RuntimeLink
            },
            integrity: KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget::new(&path, &target)?),
        });
    }
    if !plan.other_entries.is_empty() {
        return Err(KnownGoodInventoryError::UnsupportedRuntimeEntry);
    }
    if !saw_java {
        return Err(KnownGoodInventoryError::MissingRuntimeExecutable);
    }
    validate_runtime_path_tree(&entries)?;
    for entry in entries {
        builder.insert(entry)?;
    }
    Ok(())
}

pub(crate) fn runtime_inventory_from_source(
    source: &RuntimeSourceReceipt,
) -> Result<KnownGoodInventory, KnownGoodInventoryError> {
    let mut builder = InventoryBuilder::default();
    add_runtime(
        &mut builder,
        source.component(),
        source.bytes(),
        &ExpectedIntegrity {
            size: Some(source.expected_size()),
            sha1: Some(source.expected_sha1().to_string()),
        },
    )?;
    Ok(builder.finish())
}

pub(crate) fn replace_runtime_projection(
    active: &KnownGoodInventory,
    runtime_only: KnownGoodInventory,
    component: &RuntimeId,
) -> Result<KnownGoodInventory, KnownGoodInventoryError> {
    if runtime_only.entries.iter().any(|entry| {
        !matches!(
            &entry.root,
            KnownGoodRoot::ManagedRuntime { component: observed }
                if observed.as_str() == component.as_str()
        )
    }) {
        return Err(KnownGoodInventoryError::RuntimeIdentityMismatch);
    }
    if active.entries.iter().any(|entry| {
        matches!(
            &entry.root,
            KnownGoodRoot::ManagedRuntime { component: observed }
                if observed.as_str() != component.as_str()
        )
    }) {
        return Err(KnownGoodInventoryError::RuntimeIdentityMismatch);
    }
    let mut builder = InventoryBuilder::default();
    for (inventory_ordinal, entry) in active
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !matches!(entry.root, KnownGoodRoot::ManagedRuntime { .. }))
    {
        builder.insert_preserving_standalone_leaf_repair_source(
            active,
            inventory_ordinal,
            entry.clone(),
        )?;
    }
    for entry in runtime_only.entries {
        builder.insert(entry)?;
    }
    Ok(builder.finish())
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
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
    let size = expected.size.ok_or(KnownGoodInventoryError::MissingSize)?;
    expected_integrity_with_observed_size(expected, size)
}

fn authenticated_mojang_size(size: i64) -> Result<u64, KnownGoodInventoryError> {
    u64::try_from(size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or(KnownGoodInventoryError::MissingSize)
}

fn expected_integrity_with_observed_size(
    expected: &ExpectedIntegrity,
    observed_size: u64,
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
    if expected.size.is_some_and(|size| size != observed_size) {
        return Err(KnownGoodInventoryError::SizeMismatch);
    }
    let value = expected
        .sha1
        .as_deref()
        .ok_or(KnownGoodInventoryError::MissingChecksum)?;
    Ok(KnownGoodIntegrity::Sha1 {
        digest: Sha1Digest::from_metadata(value)?,
        size: observed_size,
    })
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
    entries: BTreeMap<(String, String, String), PendingInventoryEntry>,
}

struct PendingInventoryEntry {
    entry: KnownGoodEntry,
    standalone_leaf_repair_source: Option<KnownGoodStandaloneLeafRepairSourceContract>,
}

impl InventoryBuilder {
    fn insert(&mut self, entry: KnownGoodEntry) -> Result<(), KnownGoodInventoryError> {
        self.insert_with_standalone_leaf_repair_source(entry, None)
    }

    fn insert_with_standalone_leaf_repair_source(
        &mut self,
        entry: KnownGoodEntry,
        provider_url: Option<&str>,
    ) -> Result<(), KnownGoodInventoryError> {
        let key = (
            entry.root.stable_id().to_string(),
            entry.root.scope_id().to_string(),
            entry.path.as_str().to_string(),
        );
        let repair_source = provider_url
            .map(|provider_url| {
                KnownGoodStandaloneLeafRepairSourceContract::new(&entry, provider_url)
            })
            .transpose()?;
        if let Some(existing) = self.entries.get_mut(&key) {
            if existing.entry != entry {
                return Err(KnownGoodInventoryError::ConflictingEntry);
            }
            match (&existing.standalone_leaf_repair_source, repair_source) {
                (Some(existing), Some(candidate)) if existing != &candidate => {
                    return Err(KnownGoodInventoryError::ConflictingRepairSource);
                }
                (None, Some(candidate)) => {
                    existing.standalone_leaf_repair_source = Some(candidate);
                }
                _ => {}
            }
            return Ok(());
        }
        if self.entries.len() >= MAX_KNOWN_GOOD_ENTRIES {
            return Err(KnownGoodInventoryError::TooManyEntries);
        }
        self.entries.insert(
            key,
            PendingInventoryEntry {
                entry,
                standalone_leaf_repair_source: repair_source,
            },
        );
        Ok(())
    }

    fn insert_preserving_standalone_leaf_repair_source(
        &mut self,
        source_inventory: &KnownGoodInventory,
        source_inventory_ordinal: usize,
        entry: KnownGoodEntry,
    ) -> Result<(), KnownGoodInventoryError> {
        let provider_url = match source_inventory
            .standalone_leaf_repair_sources
            .get(&source_inventory_ordinal)
        {
            Some(contract) if contract.matches(&entry) => Some(contract.provider_url.as_str()),
            Some(_) => return Err(KnownGoodInventoryError::InvalidRepairSource),
            None => None,
        };
        self.insert_with_standalone_leaf_repair_source(entry, provider_url)
    }

    fn finish(self) -> KnownGoodInventory {
        let mut entries = Vec::with_capacity(self.entries.len());
        let mut standalone_leaf_repair_sources = BTreeMap::new();
        for (inventory_ordinal, pending) in self.entries.into_values().enumerate() {
            entries.push(pending.entry);
            if let Some(source) = pending.standalone_leaf_repair_source {
                standalone_leaf_repair_sources.insert(inventory_ordinal, source);
            }
        }
        KnownGoodInventory {
            entries,
            standalone_leaf_repair_sources,
        }
    }
}

impl KnownGoodStandaloneLeafRepairSourceContract {
    fn new(entry: &KnownGoodEntry, provider_url: &str) -> Result<Self, KnownGoodInventoryError> {
        let KnownGoodIntegrity::Sha1 { digest, size } = entry.integrity() else {
            return Err(KnownGoodInventoryError::InvalidRepairSource);
        };
        let supported = match (entry.root(), entry.kind()) {
            (
                KnownGoodRoot::Libraries,
                KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary,
            ) => *size > 0 && repair_provider_url_is_supported(provider_url),
            (KnownGoodRoot::Assets, KnownGoodArtifactKind::AssetIndex) => {
                *size > 0
                    && asset_index_path_is_canonical(entry.path())
                    && repair_provider_url_is_supported(provider_url)
            }
            (KnownGoodRoot::Assets, KnownGoodArtifactKind::AssetObject) => {
                entry.path().as_str() == asset_object_path(digest)
                    && provider_url == asset_object_provider_url(digest)
            }
            _ => false,
        };
        if !supported {
            return Err(KnownGoodInventoryError::InvalidRepairSource);
        }
        Ok(Self {
            root: entry.root().clone(),
            path: entry.path().clone(),
            kind: entry.kind(),
            digest: digest.clone(),
            size: *size,
            provider_url: provider_url.to_string(),
        })
    }

    fn matches(&self, entry: &KnownGoodEntry) -> bool {
        Self::new(entry, &self.provider_url).is_ok_and(|candidate| candidate.eq(self))
    }
}

fn asset_index_path_is_canonical(path: &KnownGoodRelativePath) -> bool {
    path.as_str()
        .strip_prefix("indexes/")
        .and_then(|path| path.strip_suffix(".json"))
        .is_some_and(|id| KnownGoodId::new(id).is_ok())
}

fn asset_object_path(digest: &Sha1Digest) -> String {
    format!("objects/{}/{}", &digest.as_str()[..2], digest.as_str())
}

fn asset_object_provider_url(digest: &Sha1Digest) -> String {
    format!(
        "{ASSET_OBJECT_BASE_URL}/{}/{}",
        &digest.as_str()[..2],
        digest.as_str()
    )
}

fn repair_provider_url_is_supported(provider_url: &str) -> bool {
    reqwest::Url::parse(provider_url).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::ExactLibraryDownloadProof;
    use crate::known_good_libraries::{
        LibraryAcquisition, seal_profile_exact_library_declarations,
        seal_vanilla_library_declarations_for_test,
    };
    use crate::launch::{
        ArgumentsSection, AssetIndex, Downloads, JavaVersion, LibraryArtifact, LibraryDownload,
        LoggingConf, LoggingEntry, LoggingFile,
    };
    use crate::loaders::providers::{ProfileInstallProof, ProfileLibraryProof};
    use crate::loaders::types::LoaderBuildSubjectKind;
    use crate::loaders::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderInstallSource, LoaderInstallability,
        LoaderProfileFragment, build_id_for, installed_version_id_for,
    };
    use crate::rules::Rule;
    use std::collections::HashMap;

    fn profile_receipt_fixture() -> (
        KnownGoodInstallReceipt,
        LoaderBuildRecord,
        SealedExactLibraryDeclarations,
        VersionJson,
        Vec<u8>,
    ) {
        let record = loader_record(LoaderComponentId::Fabric);
        let vanilla = fixture(false);
        let mut base_version = vanilla.version.clone();
        base_version.id = record.minecraft_version.clone();
        base_version.inherits_from.clear();
        base_version.materialized = false;
        base_version.libraries = vec![
            checksum_library(
                "org.ow2.asm:asm:9.6",
                "org/ow2/asm/asm/9.6/asm-9.6.jar",
                SHA_A,
                10,
            ),
            checksum_library(
                "com.mojang:inherited:1",
                "com/mojang/inherited/1/inherited-1.jar",
                SHA_B,
                11,
            ),
        ];
        let mut inventory = InventoryBuilder::default();
        for entry in test_base_client_inventory(&base_version).entries {
            inventory.insert(entry).expect("base client");
        }
        add_asset_index(&mut inventory, &base_version, Some(&vanilla.asset_index))
            .expect("base assets");
        for (path, digest, size) in [
            ("org/ow2/asm/asm/9.6/asm-9.6.jar", SHA_A, 10),
            ("com/mojang/inherited/1/inherited-1.jar", SHA_B, 11),
        ] {
            inventory
                .insert(KnownGoodEntry {
                    root: KnownGoodRoot::Libraries,
                    path: KnownGoodRelativePath::new(path).expect("base library path"),
                    kind: KnownGoodArtifactKind::Library,
                    integrity: KnownGoodIntegrity::Sha1 {
                        digest: Sha1Digest::from_metadata(digest).expect("base digest"),
                        size,
                    },
                })
                .expect("base library");
        }
        let base = KnownGoodInstallReceipt {
            authenticated: AuthenticatedKnownGoodReceipt {
                version_id: KnownGoodId::new(&record.minecraft_version).expect("base id"),
                inventory: inventory.finish(),
                effective_version: base_version.clone(),
                environment: crate::rules::default_environment(),
            },
        };
        let profile_libraries = vec![
            checksum_library(
                "org.ow2.asm:asm:9.9",
                "org/ow2/asm/asm/9.9/asm-9.9.jar",
                SHA_C,
                91,
            ),
            checksum_library(
                "example:profile:1",
                "example/profile/1/profile-1.jar",
                SHA_C,
                92,
            ),
        ];
        let proof = ProfileInstallProof::from_test(
            format!(
                "fabric-loader-{}-{}",
                record.loader_version, record.minecraft_version
            ),
            record.minecraft_version.clone(),
            "example.ProfileMain".to_string(),
            vec![ProfileLibraryProof::from_test(
                "org.ow2.asm:asm:9.9".to_string(),
                None,
                None,
            )],
        );
        let profile_fragment = LoaderProfileFragment {
            id: format!(
                "fabric-loader-{}-{}",
                record.loader_version, record.minecraft_version
            ),
            inherits_from: record.minecraft_version.clone(),
            kind: "release".to_string(),
            main_class: "example.ProfileMain".to_string(),
            libraries: profile_libraries,
            ..LoaderProfileFragment::default()
        };
        let pending = seal_profile_exact_library_declarations(
            profile_fragment,
            proof,
            LoaderComponentId::Fabric,
            &base.authenticated.environment,
        )
        .expect("profile declarations");
        let (libraries, environment) = pending.profile_plan_inputs().expect("profile plan inputs");
        let jobs = library_artifact_plans_for(libraries, environment)
            .expect("profile plans")
            .into_iter()
            .map(|plan| crate::download::DownloadJob {
                relative_path: plan.relative_path,
                url: plan.source_url.expect("profile URL"),
                name: plan.name,
                expected: plan.expected,
                is_native: plan.is_native,
            })
            .collect();
        let (pending, classified) = pending
            .classify_jobs(jobs)
            .expect("classified profile plans");
        let streamed = classified
            .into_iter()
            .enumerate()
            .map(|(index, classified)| {
                assert_eq!(classified.acquisition(), LibraryAcquisition::FreshStream);
                let (job, _) = classified.into_parts();
                ExactLibraryDownloadProof::new_bound_for_test(
                    job.relative_path,
                    job.is_native,
                    job.url,
                    job.expected,
                    20 + index as u64,
                    [0xc0 + index as u8; 20],
                )
            })
            .collect();
        let declarations = pending
            .seal_streamed(streamed)
            .expect("sealed profile declarations");
        let authored = declarations.profile_contract().unwrap().0;
        let resolved = compose_loader_version(
            &base_version,
            &record.minecraft_version,
            &record.version_id,
            authored,
        )
        .expect("profile composition");
        let version_bytes = serde_json::to_vec_pretty(&resolved).expect("profile version bytes");
        (base, record, declarations, resolved, version_bytes)
    }

    #[test]
    fn sha1_digest_exposes_its_canonical_exact_bytes() {
        let digest = Sha1Digest::from_metadata(&"AA".repeat(20)).expect("uppercase digest");

        assert_eq!(digest.as_str(), "aa".repeat(20));
        assert_eq!(digest.to_bytes(), [0xaa; 20]);
    }

    #[test]
    fn profile_receipt_binds_authored_recombination_and_exact_base_shadowing() {
        let (base, record, declarations, resolved, version_bytes) = profile_receipt_fixture();
        let pending = KnownGoodInstallReceipt::from_verified_profile_source(
            &base,
            &record,
            resolved,
            &version_bytes,
            declarations,
        )
        .expect("pending profile authority");
        assert_eq!(pending.version_id(), record.version_id);
        let projection = pending
            .version_bundle_projection()
            .expect("pending profile version bundle projection");
        assert_eq!(
            projection.component(),
            ManagedKnownGoodComponent::VersionBundle
        );
        assert_eq!(projection.entry_count(), 2);
        let libraries = pending
            .component_projection(ManagedKnownGoodComponent::Libraries)
            .expect("pending profile Libraries projection");
        assert_eq!(libraries.component(), ManagedKnownGoodComponent::Libraries);
        assert!(!libraries.entries().is_empty());
        assert!(libraries.entries().windows(2).all(|entries| {
            entries[0].entry().path().as_str() < entries[1].entry().path().as_str()
        }));
        assert!(libraries.entries().iter().all(|projected| {
            projected.entry().root() == &KnownGoodRoot::Libraries
                && matches!(
                    projected.entry().kind(),
                    KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary
                )
        }));
        let inventory = pending
            .seal_after_version_bundle_commit()
            .into_activation_source()
            .into_parts()
            .1;
        assert_eq!(
            inventory
                .entries()
                .iter()
                .filter(|entry| entry.path().as_str() == "com/mojang/inherited/1/inherited-1.jar")
                .count(),
            1
        );
        assert!(
            inventory
                .entries()
                .iter()
                .any(|entry| entry.path().as_str() == "org/ow2/asm/asm/9.9/asm-9.9.jar")
        );
        assert!(
            inventory
                .entries()
                .iter()
                .all(|entry| entry.path().as_str() != "org/ow2/asm/asm/9.6/asm-9.6.jar")
        );

        for mutation in 0..11 {
            let (mut base, record, declarations, mut resolved, _) = profile_receipt_fixture();
            match mutation {
                0 => resolved.libraries.push(checksum_library(
                    "example:added:1",
                    "example/added/1/added-1.jar",
                    SHA_A,
                    1,
                )),
                1 => {
                    resolved.libraries.remove(0);
                }
                2 => resolved.libraries.reverse(),
                3 => {
                    resolved.libraries[0].rules = vec![Rule {
                        action: "allow".to_string(),
                        os: None,
                        features: None,
                    }];
                }
                4 => {
                    resolved.libraries[0]
                        .downloads
                        .as_mut()
                        .and_then(|downloads| downloads.artifact.as_mut())
                        .unwrap()
                        .path = "org/ow2/asm/asm/9.9/other.jar".to_string();
                }
                5 => {
                    let classifier = crate::rules::native_classifier_key();
                    resolved.libraries[0].natives.insert(
                        base.authenticated.environment.os_name.clone(),
                        classifier.clone(),
                    );
                    resolved.libraries[0]
                        .downloads
                        .as_mut()
                        .unwrap()
                        .classifiers
                        .insert(
                            classifier,
                            LibraryArtifact {
                                path: "org/ow2/asm/asm/9.9/native.jar".to_string(),
                                sha1: SHA_A.to_string(),
                                size: 1,
                                url: "https://example.invalid/native".to_string(),
                            },
                        );
                }
                6 => {
                    resolved.libraries[0].sha1 = SHA_A.to_string();
                    resolved.libraries[0].size += 1;
                }
                7 => base.authenticated.environment.os_name = "different-os".to_string(),
                8 => resolved.main_class = "different.Main".to_string(),
                9 => resolved.kind = "snapshot".to_string(),
                10 => resolved.minecraft_arguments = "--different".to_string(),
                _ => unreachable!(),
            }
            let bytes = serde_json::to_vec_pretty(&resolved).expect("mutated profile bytes");
            assert_eq!(
                KnownGoodInstallReceipt::from_verified_profile_source(
                    &base,
                    &record,
                    resolved,
                    &bytes,
                    declarations,
                ),
                Err(KnownGoodInventoryError::ProfileLibraryProofMismatch),
                "mutation {mutation} must fail"
            );
        }
    }

    #[test]
    fn legacy_archive_receipt_is_derived_from_exact_child_sources_and_base_authority() {
        let fixture = fixture(false);
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
            authenticated: AuthenticatedKnownGoodReceipt {
                version_id: KnownGoodId::new(&fixture.version.id).expect("base id"),
                inventory: fixture.derive().expect("base inventory"),
                effective_version: fixture.version.clone(),
                environment: fixture.environment.clone(),
            },
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
        let receipt = KnownGoodInstallReceipt::from_verified_legacy_archive_source(
            &base,
            &record,
            child,
            &version_bytes,
            child_client_bytes,
        )
        .expect("legacy receipt");
        let inventory = receipt
            .seal_after_version_bundle_commit()
            .into_activation_source()
            .into_parts()
            .1;

        assert_entry(
            &inventory,
            &KnownGoodRoot::Versions,
            &format!("{0}/{0}.jar", record.version_id),
            KnownGoodArtifactKind::ClientJar,
            &KnownGoodIntegrity::Sha1 {
                digest: sha1_digest(child_client_bytes),
                size: child_client_bytes.len() as u64,
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
        let inherited_asset_index_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.kind() == KnownGoodArtifactKind::AssetIndex)
            .expect("inherited asset index ordinal");
        assert_eq!(
            inventory
                .bind_standalone_leaf_repair_source(inherited_asset_index_ordinal)
                .expect("inherited asset index source")
                .provider_url(),
            fixture.version.asset_index.url
        );
        let inherited_asset_object_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.kind() == KnownGoodArtifactKind::AssetObject)
            .expect("inherited asset object ordinal");
        assert_eq!(
            inventory
                .bind_standalone_leaf_repair_source(inherited_asset_object_ordinal)
                .expect("inherited asset object source")
                .provider_url(),
            format!("{ASSET_OBJECT_BASE_URL}/aa/{SHA_A}")
        );
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
                size: 10,
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
        let fixture = fixture(false);
        let base = KnownGoodInstallReceipt {
            authenticated: AuthenticatedKnownGoodReceipt {
                version_id: KnownGoodId::new(&fixture.version.id).expect("base id"),
                inventory: InventoryBuilder::default().finish(),
                effective_version: fixture.version,
                environment: fixture.environment,
            },
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
        let fixture = fixture(false);
        let inventory = fixture.derive().expect("vanilla inventory");

        assert_entry(
            &inventory,
            &KnownGoodRoot::Versions,
            "fixture-version/fixture-version.json",
            KnownGoodArtifactKind::VersionMetadata,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_C).unwrap(),
                size: fixture.version_metadata_size,
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Versions,
            "fixture-version/fixture-version.jar",
            KnownGoodArtifactKind::ClientJar,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).unwrap(),
                size: 40,
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "com/mojang/strict/1.0/strict-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).unwrap(),
                size: 10,
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
                size: fixture.asset_index.len() as u64,
            }
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Assets,
            &format!("objects/aa/{SHA_A}"),
            KnownGoodArtifactKind::AssetObject,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).unwrap(),
                size: 5,
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Assets,
            "log_configs/client-log.xml",
            KnownGoodArtifactKind::LogConfig,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_C).unwrap(),
                size: 15,
            },
        );
        assert_runtime_entries(&inventory);
        assert_sorted_unique(&inventory);
    }

    #[test]
    fn vanilla_library_without_declared_size_uses_same_hash_observation() {
        let mut fixture = fixture(false);
        let library = fixture
            .version
            .libraries
            .first_mut()
            .expect("fixture library");
        library.size = 0;
        library
            .downloads
            .as_mut()
            .and_then(|downloads| downloads.artifact.as_mut())
            .expect("fixture artifact")
            .size = 0;
        fixture.library_authority = seal_vanilla_library_declarations_for_test(
            &fixture.version,
            &fixture.environment,
            vec![ExactLibraryDownloadProof::new_bound_for_test(
                PortableRelativePath::new("com/mojang/strict/1.0/strict-1.0.jar")
                    .expect("library path"),
                false,
                "https://example.invalid/library".to_string(),
                ExpectedIntegrity {
                    size: None,
                    sha1: Some(SHA_A.to_string()),
                },
                10,
                [0xaa; 20],
            )],
        )
        .expect("observed library authority");

        let inventory = fixture.derive().expect("inventory");
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "com/mojang/strict/1.0/strict-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                size: 10,
            },
        );
    }

    #[test]
    fn vanilla_authority_rejects_same_path_contract_and_rule_shadowing() {
        let fixture = fixture(false);
        let mut variants = Vec::new();

        let mut digest = fixture.version.clone();
        digest.libraries[0].sha1 = SHA_B.to_string();
        digest.libraries[0]
            .downloads
            .as_mut()
            .and_then(|downloads| downloads.artifact.as_mut())
            .expect("artifact")
            .sha1 = SHA_B.to_string();
        variants.push(digest);

        let mut size = fixture.version.clone();
        size.libraries[0].size = 11;
        size.libraries[0]
            .downloads
            .as_mut()
            .and_then(|downloads| downloads.artifact.as_mut())
            .expect("artifact")
            .size = 11;
        variants.push(size);

        let mut native = fixture.version.clone();
        let path = native.libraries[0]
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .expect("artifact")
            .path
            .clone();
        native.libraries[0].downloads = Some(LibraryDownload {
            artifact: None,
            classifiers: HashMap::from([(
                "natives-linux".to_string(),
                LibraryArtifact {
                    path,
                    sha1: SHA_A.to_string(),
                    size: 10,
                    url: "https://example.invalid/library".to_string(),
                },
            )]),
        });
        native.libraries[0]
            .natives
            .insert("linux".to_string(), "natives-linux".to_string());
        variants.push(native);

        let mut rules = fixture.version.clone();
        rules.libraries[0].rules = vec![Rule {
            action: "allow".to_string(),
            os: None,
            features: None,
        }];
        variants.push(rules);

        for version in variants {
            assert_eq!(
                fixture.derive_version(&version),
                Err(KnownGoodInventoryError::VanillaLibraryProofMismatch)
            );
        }
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
    fn physical_mapping_covers_library_and_managed_runtime_roots() {
        let fixture = tempfile::tempdir().expect("physical mapping fixture");
        let runtime_cache = crate::runtime::ManagedRuntimeCache::isolated_for_test()
            .expect("isolated runtime cache");
        let library_root = &fixture.path().join("library-root");
        let make_entry = |root, path, kind, integrity| KnownGoodEntry {
            root,
            path: KnownGoodRelativePath::new(path).expect("safe mapped path"),
            kind,
            integrity,
        };
        let file_integrity = || KnownGoodIntegrity::Sha1 {
            digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
            size: 10,
        };
        let cases = [
            (
                make_entry(
                    KnownGoodRoot::Versions,
                    "1.21.5/1.21.5.jar",
                    KnownGoodArtifactKind::ClientJar,
                    file_integrity(),
                ),
                library_root.join("versions/1.21.5/1.21.5.jar"),
            ),
            (
                make_entry(
                    KnownGoodRoot::Libraries,
                    "com/example/library.jar",
                    KnownGoodArtifactKind::Library,
                    file_integrity(),
                ),
                library_root.join("libraries/com/example/library.jar"),
            ),
            (
                make_entry(
                    KnownGoodRoot::Assets,
                    "indexes/1.21.json",
                    KnownGoodArtifactKind::AssetIndex,
                    file_integrity(),
                ),
                library_root.join("assets/indexes/1.21.json"),
            ),
        ];
        for (entry, expected) in cases {
            assert_eq!(
                known_good_entry_path(library_root, &runtime_cache, &entry).absolute(),
                expected
            );
        }

        let runtime_root = KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").expect("runtime id"),
        };
        for (path, kind, integrity) in [
            (
                "bin",
                KnownGoodArtifactKind::RuntimeDirectory,
                KnownGoodIntegrity::Directory,
            ),
            (
                "bin/java",
                KnownGoodArtifactKind::RuntimeExecutable,
                file_integrity(),
            ),
            (
                ".axial-ready",
                KnownGoodArtifactKind::RuntimeReadyMarker,
                KnownGoodIntegrity::ExactBytes {
                    digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                    size: 5,
                },
            ),
            (
                COMPONENT_MANIFEST_PROOF_FILE,
                KnownGoodArtifactKind::RuntimeManifestProof,
                file_integrity(),
            ),
            (
                "java-link",
                KnownGoodArtifactKind::RuntimeLink,
                KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget("bin/java".to_string())),
            ),
        ] {
            let entry = make_entry(runtime_root.clone(), path, kind, integrity);
            assert_eq!(
                known_good_entry_path(library_root, &runtime_cache, &entry).absolute(),
                runtime_cache.root().join("java-runtime-delta").join(path)
            );
        }
    }

    #[test]
    fn tier_zero_selection_is_launch_critical_only() {
        for kind in [
            KnownGoodArtifactKind::VersionMetadata,
            KnownGoodArtifactKind::ClientJar,
            KnownGoodArtifactKind::Library,
            KnownGoodArtifactKind::NativeLibrary,
            KnownGoodArtifactKind::AssetIndex,
            KnownGoodArtifactKind::LogConfig,
            KnownGoodArtifactKind::RuntimeManifestProof,
            KnownGoodArtifactKind::RuntimeReadyMarker,
            KnownGoodArtifactKind::RuntimeExecutable,
        ] {
            assert!(kind.needed_for_launch_tier0(), "{}", kind.stable_id());
        }
        for kind in [
            KnownGoodArtifactKind::AssetObject,
            KnownGoodArtifactKind::RuntimeFile,
            KnownGoodArtifactKind::RuntimeDirectory,
            KnownGoodArtifactKind::RuntimeLink,
        ] {
            assert!(!kind.needed_for_launch_tier0(), "{}", kind.stable_id());
        }
    }

    #[test]
    fn tier_zero_projection_tracks_the_selected_launch_runtime() {
        let file = |root, path, kind| KnownGoodEntry {
            root,
            path: KnownGoodRelativePath::new(path).expect("path"),
            kind,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                size: 10,
            },
        };
        let inventory = KnownGoodInventory {
            entries: vec![
                file(
                    KnownGoodRoot::Versions,
                    "1.21.1/1.21.1.jar",
                    KnownGoodArtifactKind::ClientJar,
                ),
                file(
                    KnownGoodRoot::ManagedRuntime {
                        component: KnownGoodId::new("java-runtime-delta").expect("component"),
                    },
                    "bin/java",
                    KnownGoodArtifactKind::RuntimeExecutable,
                ),
                file(
                    KnownGoodRoot::ManagedRuntime {
                        component: KnownGoodId::new("java-runtime-epsilon").expect("component"),
                    },
                    "bin/java",
                    KnownGoodArtifactKind::RuntimeExecutable,
                ),
            ],
            standalone_leaf_repair_sources: BTreeMap::new(),
        };

        assert_eq!(
            inventory
                .launch_tier0_projection(LaunchTier0RuntimeSelection::PreferredManaged)
                .expect("preferred projection")
                .len(),
            3
        );
        let component = inventory
            .launch_tier0_projection(LaunchTier0RuntimeSelection::ManagedComponent(
                "java-runtime-epsilon",
            ))
            .expect("component projection");
        assert_eq!(component.len(), 2);
        assert!(matches!(
            component[1].1.root(),
            KnownGoodRoot::ManagedRuntime { component }
                if component.as_str() == "java-runtime-epsilon"
        ));
        assert_eq!(
            inventory
                .launch_tier0_projection(LaunchTier0RuntimeSelection::ExternalExecutable)
                .expect("external projection")
                .len(),
            1
        );
    }

    #[test]
    fn tier_zero_projection_fails_closed_above_hard_bound() {
        let entry = || KnownGoodEntry {
            root: KnownGoodRoot::Libraries,
            path: KnownGoodRelativePath::new("bounded/library.jar").expect("path"),
            kind: KnownGoodArtifactKind::Library,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                size: 10,
            },
        };
        let inventory = KnownGoodInventory {
            entries: (0..=MAX_LAUNCH_TIER0_ENTRIES).map(|_| entry()).collect(),
            standalone_leaf_repair_sources: BTreeMap::new(),
        };
        assert_eq!(
            inventory
                .launch_tier0_projection(LaunchTier0RuntimeSelection::PreferredManaged)
                .expect_err("oversized projection")
                .selected_entry_count(),
            MAX_LAUNCH_TIER0_ENTRIES + 1
        );
    }

    fn tier_one_entry(
        root: KnownGoodRoot,
        path: &str,
        kind: KnownGoodArtifactKind,
        integrity: KnownGoodIntegrity,
    ) -> KnownGoodEntry {
        KnownGoodEntry {
            root,
            path: KnownGoodRelativePath::new(path).expect("tier one test path"),
            kind,
            integrity,
        }
    }

    fn tier_one_sha1_entry(
        root: KnownGoodRoot,
        path: &str,
        kind: KnownGoodArtifactKind,
        size: u64,
    ) -> KnownGoodEntry {
        tier_one_entry(
            root,
            path,
            kind,
            KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                size,
            },
        )
    }

    #[test]
    fn tier_one_projection_is_closed_to_managed_client_library_and_native_files() {
        let runtime_root = || KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").expect("component"),
        };
        let inventory = KnownGoodInventory {
            entries: vec![
                tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "indexes/1.json",
                    KnownGoodArtifactKind::AssetIndex,
                    1,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Libraries,
                    "com/example/library.jar",
                    KnownGoodArtifactKind::Library,
                    11,
                ),
                tier_one_entry(
                    KnownGoodRoot::Versions,
                    "1.21.1/1.21.1.jar",
                    KnownGoodArtifactKind::ClientJar,
                    KnownGoodIntegrity::ExactBytes {
                        digest: Sha1Digest::from_metadata(SHA_B).expect("digest"),
                        size: 13,
                    },
                ),
                tier_one_sha1_entry(
                    runtime_root(),
                    "bin/java",
                    KnownGoodArtifactKind::RuntimeExecutable,
                    15,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Libraries,
                    "com/example/native.jar",
                    KnownGoodArtifactKind::NativeLibrary,
                    17,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Versions,
                    "1.21.1/log.xml",
                    KnownGoodArtifactKind::LogConfig,
                    19,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "objects/spoofed-client.jar",
                    KnownGoodArtifactKind::ClientJar,
                    23,
                ),
                tier_one_sha1_entry(
                    runtime_root(),
                    "lib/spoofed-library.jar",
                    KnownGoodArtifactKind::Library,
                    29,
                ),
            ],
            standalone_leaf_repair_sources: BTreeMap::new(),
        };

        let projection = inventory
            .launch_tier1_projection()
            .expect("bounded tier one projection");
        let projected = projection
            .into_entries()
            .into_iter()
            .map(LaunchTier1ProjectionEntry::into_parts)
            .collect::<Vec<_>>();
        assert_eq!(
            projected
                .iter()
                .map(|(inventory_ordinal, _)| *inventory_ordinal)
                .collect::<Vec<_>>(),
            [1, 2, 4]
        );
        assert_eq!(
            projected
                .iter()
                .map(|(_, file)| file.root().clone())
                .collect::<Vec<_>>(),
            [
                KnownGoodRoot::Libraries,
                KnownGoodRoot::Versions,
                KnownGoodRoot::Libraries,
            ]
        );
        assert_eq!(
            projected
                .iter()
                .map(|(_, file)| file.kind())
                .collect::<Vec<_>>(),
            [
                KnownGoodArtifactKind::Library,
                KnownGoodArtifactKind::ClientJar,
                KnownGoodArtifactKind::NativeLibrary,
            ]
        );
        assert_eq!(
            projected
                .iter()
                .map(|(_, file)| file.path().as_str())
                .collect::<Vec<_>>(),
            [
                "com/example/library.jar",
                "1.21.1/1.21.1.jar",
                "com/example/native.jar",
            ]
        );
        assert_eq!(
            projected
                .iter()
                .map(|(_, file)| (file.digest().as_str(), file.size()))
                .collect::<Vec<_>>(),
            [(SHA_A, 11), (SHA_B, 13), (SHA_A, 17)]
        );
        assert_eq!(
            projected.iter().map(|(_, file)| file.size()).sum::<u64>(),
            41
        );
        let physical = projected[1].1.physical_path(Path::new("/managed/library"));
        assert_eq!(physical.root(), Path::new("/managed/library"));
        assert_eq!(physical.relative(), Path::new("versions/1.21.1/1.21.1.jar"));
    }

    #[test]
    fn tier_one_projection_excludes_every_other_artifact_kind() {
        let runtime_root = || KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").expect("component"),
        };
        let entries = [
            (
                KnownGoodRoot::Versions,
                KnownGoodArtifactKind::VersionMetadata,
            ),
            (KnownGoodRoot::Assets, KnownGoodArtifactKind::AssetIndex),
            (KnownGoodRoot::Assets, KnownGoodArtifactKind::AssetObject),
            (KnownGoodRoot::Versions, KnownGoodArtifactKind::LogConfig),
            (runtime_root(), KnownGoodArtifactKind::RuntimeManifestProof),
            (runtime_root(), KnownGoodArtifactKind::RuntimeReadyMarker),
            (runtime_root(), KnownGoodArtifactKind::RuntimeFile),
            (runtime_root(), KnownGoodArtifactKind::RuntimeExecutable),
            (runtime_root(), KnownGoodArtifactKind::RuntimeDirectory),
            (runtime_root(), KnownGoodArtifactKind::RuntimeLink),
        ]
        .into_iter()
        .enumerate()
        .map(|(ordinal, (root, kind))| {
            tier_one_sha1_entry(root, &format!("excluded/{ordinal}"), kind, 1)
        })
        .collect();

        let projection = KnownGoodInventory {
            entries,
            standalone_leaf_repair_sources: BTreeMap::new(),
        }
        .launch_tier1_projection()
        .expect("excluded kinds cannot invalidate projection");
        assert_eq!(projection.into_entries().len(), 0);
    }

    #[test]
    fn tier_one_projection_admits_exact_entry_bound_and_refuses_above_it() {
        let entries = (0..MAX_LAUNCH_TIER1_ENTRIES)
            .map(|ordinal| {
                tier_one_sha1_entry(
                    KnownGoodRoot::Libraries,
                    &format!("bounded/library-{ordinal}.jar"),
                    KnownGoodArtifactKind::Library,
                    0,
                )
            })
            .collect::<Vec<_>>();
        let exact = KnownGoodInventory {
            entries: entries.clone(),
            standalone_leaf_repair_sources: BTreeMap::new(),
        }
        .launch_tier1_projection()
        .expect("exact entry bound");
        assert_eq!(exact.into_entries().len(), MAX_LAUNCH_TIER1_ENTRIES);

        let mut oversized = entries;
        oversized.push(tier_one_sha1_entry(
            KnownGoodRoot::Libraries,
            "bounded/overflow.jar",
            KnownGoodArtifactKind::Library,
            0,
        ));
        assert_eq!(
            KnownGoodInventory {
                entries: oversized,
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .launch_tier1_projection()
            .expect_err("entry bound must be closed"),
            LaunchTier1ProjectionError::TooManyEntries {
                selected_entry_count: MAX_LAUNCH_TIER1_ENTRIES + 1,
            }
        );
    }

    #[test]
    fn tier_one_projection_counts_large_matching_set_before_admission() {
        let selected_entry_count = MAX_LAUNCH_TIER1_ENTRIES * 8 + 37;
        let entries = (0..selected_entry_count)
            .map(|ordinal| {
                tier_one_entry(
                    KnownGoodRoot::Libraries,
                    &format!("large/library-{ordinal}.jar"),
                    KnownGoodArtifactKind::Library,
                    if ordinal == 0 {
                        KnownGoodIntegrity::Directory
                    } else {
                        KnownGoodIntegrity::Sha1 {
                            digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                            size: 0,
                        }
                    },
                )
            })
            .collect();

        assert_eq!(
            KnownGoodInventory {
                entries,
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .launch_tier1_projection()
            .expect_err("large matching projection must refuse before admission"),
            LaunchTier1ProjectionError::TooManyEntries {
                selected_entry_count,
            }
        );
    }

    #[test]
    fn tier_one_projection_refuses_non_file_integrity() {
        for integrity in [
            KnownGoodIntegrity::Directory,
            KnownGoodIntegrity::LinkTarget(
                KnownGoodLinkTarget::new("linked.jar", "target.jar").expect("link target"),
            ),
        ] {
            let inventory = KnownGoodInventory {
                entries: vec![
                    tier_one_sha1_entry(
                        KnownGoodRoot::Assets,
                        "objects/excluded",
                        KnownGoodArtifactKind::AssetObject,
                        1,
                    ),
                    tier_one_entry(
                        KnownGoodRoot::Libraries,
                        "linked.jar",
                        KnownGoodArtifactKind::Library,
                        integrity,
                    ),
                ],
                standalone_leaf_repair_sources: BTreeMap::new(),
            };
            assert_eq!(
                inventory
                    .launch_tier1_projection()
                    .expect_err("non-file integrity must be refused"),
                LaunchTier1ProjectionError::UnsupportedIntegrity {
                    selected_entry_count: 1,
                    inventory_ordinal: 1,
                }
            );
        }
    }

    #[test]
    fn tier_one_projection_enforces_per_artifact_byte_bound() {
        let exact = KnownGoodInventory {
            entries: vec![tier_one_sha1_entry(
                KnownGoodRoot::Versions,
                "bounded/client.jar",
                KnownGoodArtifactKind::ClientJar,
                MAX_LAUNCH_TIER1_ARTIFACT_BYTES,
            )],
            standalone_leaf_repair_sources: BTreeMap::new(),
        }
        .launch_tier1_projection()
        .expect("exact artifact byte bound");
        let (_, exact) = exact
            .into_entries()
            .into_iter()
            .next()
            .expect("projected file")
            .into_parts();
        assert_eq!(exact.size(), MAX_LAUNCH_TIER1_ARTIFACT_BYTES);

        let oversized = MAX_LAUNCH_TIER1_ARTIFACT_BYTES + 1;
        let error = KnownGoodInventory {
            entries: vec![tier_one_sha1_entry(
                KnownGoodRoot::Versions,
                "bounded/client.jar",
                KnownGoodArtifactKind::ClientJar,
                oversized,
            )],
            standalone_leaf_repair_sources: BTreeMap::new(),
        }
        .launch_tier1_projection()
        .expect_err("artifact byte bound must be closed");
        assert_eq!(
            error,
            LaunchTier1ProjectionError::ArtifactByteLimitExceeded {
                selected_entry_count: 1,
                inventory_ordinal: 0,
                expected_byte_count: oversized,
            }
        );
        assert_eq!(error.selected_entry_count(), 1);
    }

    #[test]
    fn tier_one_projection_enforces_aggregate_byte_bound() {
        let entries = |count| {
            (0..count)
                .map(|ordinal| {
                    tier_one_sha1_entry(
                        KnownGoodRoot::Libraries,
                        &format!("aggregate/library-{ordinal}.jar"),
                        KnownGoodArtifactKind::Library,
                        MAX_LAUNCH_TIER1_ARTIFACT_BYTES,
                    )
                })
                .collect::<Vec<_>>()
        };
        let exact = KnownGoodInventory {
            entries: entries(4),
            standalone_leaf_repair_sources: BTreeMap::new(),
        }
        .launch_tier1_projection()
        .expect("exact aggregate byte bound");
        assert_eq!(
            exact
                .into_entries()
                .into_iter()
                .map(LaunchTier1ProjectionEntry::into_parts)
                .map(|(_, file)| file.size())
                .sum::<u64>(),
            MAX_LAUNCH_TIER1_AGGREGATE_BYTES
        );

        let expected_byte_count = MAX_LAUNCH_TIER1_AGGREGATE_BYTES
            .checked_add(MAX_LAUNCH_TIER1_ARTIFACT_BYTES)
            .expect("bounded test total");
        assert_eq!(
            KnownGoodInventory {
                entries: entries(5),
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .launch_tier1_projection()
            .expect_err("aggregate byte bound must be closed"),
            LaunchTier1ProjectionError::AggregateByteLimitExceeded {
                selected_entry_count: 5,
                expected_byte_count,
            }
        );
    }

    #[test]
    fn managed_component_kind_mapping_is_total_and_closed() {
        for (kind, expected) in [
            (
                KnownGoodArtifactKind::VersionMetadata,
                Some(ManagedKnownGoodComponent::VersionBundle),
            ),
            (
                KnownGoodArtifactKind::ClientJar,
                Some(ManagedKnownGoodComponent::VersionBundle),
            ),
            (
                KnownGoodArtifactKind::LogConfig,
                Some(ManagedKnownGoodComponent::VersionBundle),
            ),
            (
                KnownGoodArtifactKind::Library,
                Some(ManagedKnownGoodComponent::Libraries),
            ),
            (
                KnownGoodArtifactKind::NativeLibrary,
                Some(ManagedKnownGoodComponent::Libraries),
            ),
            (
                KnownGoodArtifactKind::AssetIndex,
                Some(ManagedKnownGoodComponent::Assets),
            ),
            (
                KnownGoodArtifactKind::AssetObject,
                Some(ManagedKnownGoodComponent::Assets),
            ),
            (KnownGoodArtifactKind::RuntimeManifestProof, None),
            (KnownGoodArtifactKind::RuntimeReadyMarker, None),
            (KnownGoodArtifactKind::RuntimeFile, None),
            (KnownGoodArtifactKind::RuntimeExecutable, None),
            (KnownGoodArtifactKind::RuntimeDirectory, None),
            (KnownGoodArtifactKind::RuntimeLink, None),
        ] {
            assert_eq!(managed_component_for_kind(kind), expected);
        }
    }

    #[test]
    fn managed_component_projections_are_sorted_exact_and_component_local() {
        let runtime_root = KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").expect("component"),
        };
        let inventory = KnownGoodInventory {
            entries: vec![
                tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "log_configs/client.xml",
                    KnownGoodArtifactKind::LogConfig,
                    3,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Versions,
                    "1.21.1/1.21.1.jar",
                    KnownGoodArtifactKind::ClientJar,
                    2,
                ),
                tier_one_entry(
                    KnownGoodRoot::Versions,
                    "1.21.1/1.21.1.json",
                    KnownGoodArtifactKind::VersionMetadata,
                    KnownGoodIntegrity::ExactBytes {
                        digest: Sha1Digest::from_metadata(SHA_B).expect("digest"),
                        size: 1,
                    },
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Libraries,
                    "com/example/library.jar",
                    KnownGoodArtifactKind::Library,
                    4,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "objects/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    KnownGoodArtifactKind::AssetObject,
                    5,
                ),
                tier_one_sha1_entry(
                    runtime_root,
                    "bin/java",
                    KnownGoodArtifactKind::RuntimeExecutable,
                    6,
                ),
            ],
            standalone_leaf_repair_sources: BTreeMap::new(),
        };

        let version = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("version bundle projection");
        assert_eq!(
            version.component(),
            ManagedKnownGoodComponent::VersionBundle
        );
        assert_eq!(version.entry_count(), 3);
        assert_eq!(version.expected_content_byte_count(), 6);
        assert_eq!(
            version
                .entries()
                .iter()
                .copied()
                .map(ManagedComponentProjectionEntry::inventory_ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
        assert_eq!(
            version
                .entries()
                .iter()
                .map(|projected| projected.entry().root().clone())
                .collect::<Vec<_>>(),
            vec![
                KnownGoodRoot::Versions,
                KnownGoodRoot::Versions,
                KnownGoodRoot::Assets,
            ]
        );

        let libraries = inventory
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .expect("libraries projection");
        assert_eq!(libraries.entry_count(), 1);
        assert_eq!(
            libraries.entries()[0].entry().kind(),
            KnownGoodArtifactKind::Library
        );

        let assets = inventory
            .managed_component_projection(ManagedKnownGoodComponent::Assets)
            .expect("assets projection");
        assert_eq!(assets.entry_count(), 1);
        assert_eq!(
            assets.entries()[0].entry().kind(),
            KnownGoodArtifactKind::AssetObject
        );
    }

    #[test]
    fn managed_component_projection_rejects_selected_cross_root_kinds() {
        for (component, root, kind) in [
            (
                ManagedKnownGoodComponent::VersionBundle,
                KnownGoodRoot::Assets,
                KnownGoodArtifactKind::VersionMetadata,
            ),
            (
                ManagedKnownGoodComponent::VersionBundle,
                KnownGoodRoot::Versions,
                KnownGoodArtifactKind::LogConfig,
            ),
            (
                ManagedKnownGoodComponent::Libraries,
                KnownGoodRoot::Assets,
                KnownGoodArtifactKind::Library,
            ),
            (
                ManagedKnownGoodComponent::Assets,
                KnownGoodRoot::Libraries,
                KnownGoodArtifactKind::AssetObject,
            ),
        ] {
            assert_eq!(
                KnownGoodInventory {
                    entries: vec![tier_one_sha1_entry(root, "private/entry", kind, 1)],
                    standalone_leaf_repair_sources: BTreeMap::new(),
                }
                .managed_component_projection(component)
                .expect_err("cross-root selected entry must be refused"),
                ManagedComponentProjectionError::UnsupportedRootKind {
                    selected_entry_count: 1,
                    inventory_ordinal: 0,
                }
            );
        }
    }

    #[test]
    fn managed_component_projection_rejects_non_file_integrity() {
        for (component, root, kind, integrity) in [
            (
                ManagedKnownGoodComponent::VersionBundle,
                KnownGoodRoot::Versions,
                KnownGoodArtifactKind::VersionMetadata,
                KnownGoodIntegrity::Directory,
            ),
            (
                ManagedKnownGoodComponent::Libraries,
                KnownGoodRoot::Libraries,
                KnownGoodArtifactKind::Library,
                KnownGoodIntegrity::LinkTarget(
                    KnownGoodLinkTarget::new("entry", "target").expect("link target"),
                ),
            ),
            (
                ManagedKnownGoodComponent::Assets,
                KnownGoodRoot::Assets,
                KnownGoodArtifactKind::AssetIndex,
                KnownGoodIntegrity::Directory,
            ),
        ] {
            assert_eq!(
                KnownGoodInventory {
                    entries: vec![tier_one_entry(root, "entry", kind, integrity)],
                    standalone_leaf_repair_sources: BTreeMap::new(),
                }
                .managed_component_projection(component)
                .expect_err("non-file component entry must be refused"),
                ManagedComponentProjectionError::UnsupportedIntegrity {
                    selected_entry_count: 1,
                    inventory_ordinal: 0,
                }
            );
        }
    }

    #[test]
    fn managed_component_projection_rejects_duplicate_and_ancestor_paths() {
        let entry = tier_one_sha1_entry(
            KnownGoodRoot::Libraries,
            "shared/library.jar",
            KnownGoodArtifactKind::Library,
            1,
        );
        assert_eq!(
            KnownGoodInventory {
                entries: vec![entry.clone(), entry],
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .expect_err("duplicate path must be refused"),
            ManagedComponentProjectionError::PathCollision {
                selected_entry_count: 2,
                first_inventory_ordinal: 0,
                second_inventory_ordinal: 1,
            }
        );

        assert_eq!(
            KnownGoodInventory {
                entries: vec![
                    tier_one_sha1_entry(
                        KnownGoodRoot::Libraries,
                        "shared",
                        KnownGoodArtifactKind::Library,
                        1,
                    ),
                    tier_one_sha1_entry(
                        KnownGoodRoot::Libraries,
                        "shared/library.jar",
                        KnownGoodArtifactKind::Library,
                        1,
                    ),
                ],
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .expect_err("file ancestor must be refused"),
            ManagedComponentProjectionError::PathCollision {
                selected_entry_count: 2,
                first_inventory_ordinal: 0,
                second_inventory_ordinal: 1,
            }
        );
    }

    #[test]
    fn managed_component_projection_enforces_tier_two_bounds() {
        let bounded = tier_one_sha1_entry(
            KnownGoodRoot::Libraries,
            "bounded/library.jar",
            KnownGoodArtifactKind::Library,
            0,
        );
        assert_eq!(
            KnownGoodInventory {
                entries: vec![bounded; MAX_TIER2_ENTRIES + 1],
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .expect_err("entry bound must be refused"),
            ManagedComponentProjectionError::TooManyEntries {
                selected_entry_count: MAX_TIER2_ENTRIES + 1,
            }
        );

        let oversized = MAX_TIER2_ARTIFACT_BYTES + 1;
        assert_eq!(
            KnownGoodInventory {
                entries: vec![tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "objects/aa/oversized",
                    KnownGoodArtifactKind::AssetObject,
                    oversized,
                )],
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .managed_component_projection(ManagedKnownGoodComponent::Assets)
            .expect_err("artifact byte bound must be refused"),
            ManagedComponentProjectionError::ArtifactByteLimitExceeded {
                selected_entry_count: 1,
                inventory_ordinal: 0,
                expected_byte_count: oversized,
            }
        );

        let entry_count = usize::try_from(MAX_TIER2_AGGREGATE_BYTES / MAX_TIER2_ARTIFACT_BYTES)
            .expect("bounded entry count");
        let entries = (0..entry_count)
            .map(|ordinal| {
                tier_one_sha1_entry(
                    KnownGoodRoot::Libraries,
                    &format!("aggregate/library-{ordinal}.jar"),
                    KnownGoodArtifactKind::Library,
                    MAX_TIER2_ARTIFACT_BYTES,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            KnownGoodInventory {
                entries: entries.clone(),
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .expect("exact aggregate bound")
            .expected_content_byte_count(),
            MAX_TIER2_AGGREGATE_BYTES
        );
        let mut above = entries;
        above.push(tier_one_sha1_entry(
            KnownGoodRoot::Libraries,
            "aggregate/overflow.jar",
            KnownGoodArtifactKind::Library,
            1,
        ));
        assert_eq!(
            KnownGoodInventory {
                entries: above,
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .expect_err("aggregate byte bound must be refused"),
            ManagedComponentProjectionError::AggregateByteLimitExceeded {
                selected_entry_count: entry_count + 1,
                expected_byte_count: MAX_TIER2_AGGREGATE_BYTES + 1,
            }
        );
    }

    #[test]
    fn tier_two_projection_borrows_every_exact_launcher_owned_entry() {
        let runtime_root = || KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").expect("component"),
        };
        let exact_bytes = |size| KnownGoodIntegrity::ExactBytes {
            digest: Sha1Digest::from_metadata(SHA_B).expect("digest"),
            size,
        };
        let directory = KnownGoodIntegrity::Directory;
        let link = |path: &str, target: &str| {
            KnownGoodIntegrity::LinkTarget(
                KnownGoodLinkTarget::new(path, target).expect("link target"),
            )
        };
        let inventory = KnownGoodInventory {
            entries: vec![
                tier_one_entry(
                    KnownGoodRoot::Versions,
                    "1.21.1/1.21.1.json",
                    KnownGoodArtifactKind::VersionMetadata,
                    exact_bytes(1),
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Versions,
                    "1.21.1/1.21.1.jar",
                    KnownGoodArtifactKind::ClientJar,
                    2,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Libraries,
                    "com/example/library.jar",
                    KnownGoodArtifactKind::Library,
                    3,
                ),
                tier_one_entry(
                    KnownGoodRoot::Libraries,
                    "com/example/native.jar",
                    KnownGoodArtifactKind::NativeLibrary,
                    exact_bytes(4),
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "indexes/1.21.json",
                    KnownGoodArtifactKind::AssetIndex,
                    5,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "objects/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    KnownGoodArtifactKind::AssetObject,
                    6,
                ),
                tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "log_configs/client.xml",
                    KnownGoodArtifactKind::LogConfig,
                    7,
                ),
                tier_one_entry(
                    runtime_root(),
                    "component-manifest.json",
                    KnownGoodArtifactKind::RuntimeManifestProof,
                    exact_bytes(8),
                ),
                tier_one_entry(
                    runtime_root(),
                    ".axial-ready",
                    KnownGoodArtifactKind::RuntimeReadyMarker,
                    exact_bytes(9),
                ),
                tier_one_sha1_entry(
                    runtime_root(),
                    "lib/runtime.bin",
                    KnownGoodArtifactKind::RuntimeFile,
                    10,
                ),
                tier_one_sha1_entry(
                    runtime_root(),
                    "bin/java",
                    KnownGoodArtifactKind::RuntimeExecutable,
                    11,
                ),
                tier_one_entry(
                    runtime_root(),
                    "legal",
                    KnownGoodArtifactKind::RuntimeDirectory,
                    directory,
                ),
                tier_one_entry(
                    runtime_root(),
                    "bin/tool",
                    KnownGoodArtifactKind::RuntimeLink,
                    link("bin/tool", "../lib/runtime.bin"),
                ),
                tier_one_entry(
                    runtime_root(),
                    "bin/java-link",
                    KnownGoodArtifactKind::RuntimeExecutable,
                    link("bin/java-link", "java"),
                ),
            ],
            standalone_leaf_repair_sources: BTreeMap::new(),
        };

        let projection = inventory.tier2_projection().expect("tier two projection");
        assert_eq!(projection.entry_count(), inventory.entries().len());
        assert_eq!(projection.expected_content_byte_count(), 66);
        let projected = projection.iter().collect::<Vec<_>>();
        assert_eq!(projected.len(), inventory.entries().len());
        assert!(projected.iter().enumerate().all(|(ordinal, projected)| {
            projected.inventory_ordinal() == ordinal
                && std::ptr::eq(projected.entry(), &inventory.entries()[ordinal])
        }));

        let library_root = Path::new("/managed/library");
        let runtime_cache = crate::runtime::ManagedRuntimeCache::isolated_for_test()
            .expect("isolated runtime cache");
        let version = projected[0].physical_path(library_root, &runtime_cache);
        assert_eq!(version.root(), library_root);
        assert_eq!(version.relative(), Path::new("versions/1.21.1/1.21.1.json"));
        let library = projected[2].physical_path(library_root, &runtime_cache);
        assert_eq!(library.root(), library_root);
        assert_eq!(
            library.relative(),
            Path::new("libraries/com/example/library.jar")
        );
        let asset = projected[4].physical_path(library_root, &runtime_cache);
        assert_eq!(asset.root(), library_root);
        assert_eq!(asset.relative(), Path::new("assets/indexes/1.21.json"));
        let runtime = projected[9].physical_path(library_root, &runtime_cache);
        assert_eq!(runtime.root(), runtime_cache.root());
        assert_eq!(
            runtime.relative(),
            Path::new("java-runtime-delta/lib/runtime.bin")
        );
    }

    #[test]
    fn tier_two_projection_rejects_every_cross_root_artifact_kind() {
        let runtime_root = KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").expect("component"),
        };
        for (root, kind) in [
            (KnownGoodRoot::Versions, KnownGoodArtifactKind::Library),
            (KnownGoodRoot::Libraries, KnownGoodArtifactKind::ClientJar),
            (
                KnownGoodRoot::Assets,
                KnownGoodArtifactKind::VersionMetadata,
            ),
            (runtime_root, KnownGoodArtifactKind::AssetObject),
        ] {
            let inventory = KnownGoodInventory {
                entries: vec![tier_one_sha1_entry(root, "private/entry", kind, 1)],
                standalone_leaf_repair_sources: BTreeMap::new(),
            };
            let error = inventory
                .tier2_projection()
                .expect_err("cross-root kind must be refused");
            assert_eq!(
                error,
                Tier2ProjectionError::UnsupportedRootKind {
                    entry_count: 1,
                    inventory_ordinal: 0,
                }
            );
            assert_eq!(error.entry_count(), 1);
            let debug = format!("{error:?}");
            assert!(!debug.contains("private"));
            assert!(!debug.contains(SHA_A));
        }
    }

    #[test]
    fn tier_two_projection_rejects_wrong_integrity_for_each_entry_shape() {
        let runtime_root = || KnownGoodRoot::ManagedRuntime {
            component: KnownGoodId::new("java-runtime-delta").expect("component"),
        };
        let link = || {
            KnownGoodIntegrity::LinkTarget(
                KnownGoodLinkTarget::new("entry", "target").expect("link target"),
            )
        };
        let sha1 = || KnownGoodIntegrity::Sha1 {
            digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
            size: 1,
        };
        for (root, kind, integrity) in [
            (
                KnownGoodRoot::Versions,
                KnownGoodArtifactKind::VersionMetadata,
                KnownGoodIntegrity::Directory,
            ),
            (
                KnownGoodRoot::Libraries,
                KnownGoodArtifactKind::Library,
                link(),
            ),
            (
                KnownGoodRoot::Assets,
                KnownGoodArtifactKind::AssetIndex,
                KnownGoodIntegrity::Directory,
            ),
            (
                runtime_root(),
                KnownGoodArtifactKind::RuntimeDirectory,
                sha1(),
            ),
            (runtime_root(), KnownGoodArtifactKind::RuntimeLink, sha1()),
            (
                runtime_root(),
                KnownGoodArtifactKind::RuntimeFile,
                KnownGoodIntegrity::Directory,
            ),
            (
                runtime_root(),
                KnownGoodArtifactKind::RuntimeExecutable,
                KnownGoodIntegrity::Directory,
            ),
            (
                runtime_root(),
                KnownGoodArtifactKind::RuntimeManifestProof,
                link(),
            ),
        ] {
            let inventory = KnownGoodInventory {
                entries: vec![tier_one_entry(root, "entry", kind, integrity)],
                standalone_leaf_repair_sources: BTreeMap::new(),
            };
            assert_eq!(
                inventory
                    .tier2_projection()
                    .expect_err("wrong integrity must be refused"),
                Tier2ProjectionError::UnsupportedIntegrity {
                    entry_count: 1,
                    inventory_ordinal: 0,
                }
            );
        }
    }

    #[test]
    fn tier_two_projection_enforces_exact_entry_bound_without_a_second_entry_list() {
        let entry = tier_one_sha1_entry(
            KnownGoodRoot::Libraries,
            "bounded/library.jar",
            KnownGoodArtifactKind::Library,
            0,
        );
        let entries = vec![entry; MAX_TIER2_ENTRIES];
        let exact = KnownGoodInventory {
            entries: entries.clone(),
            standalone_leaf_repair_sources: BTreeMap::new(),
        };
        let projection = exact.tier2_projection().expect("exact entry bound");
        assert_eq!(projection.entry_count(), MAX_TIER2_ENTRIES);
        assert_eq!(projection.iter().len(), MAX_TIER2_ENTRIES);
        assert!(std::ptr::eq(
            projection.iter().next().expect("first entry").entry(),
            &exact.entries()[0]
        ));

        let mut oversized = entries;
        oversized.push(tier_one_sha1_entry(
            KnownGoodRoot::Libraries,
            "bounded/overflow.jar",
            KnownGoodArtifactKind::Library,
            0,
        ));
        assert_eq!(
            KnownGoodInventory {
                entries: oversized,
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .tier2_projection()
            .expect_err("entry bound must be closed"),
            Tier2ProjectionError::TooManyEntries {
                entry_count: MAX_TIER2_ENTRIES + 1,
            }
        );
    }

    #[test]
    fn tier_two_projection_enforces_file_and_aggregate_byte_bounds() {
        let exact_file = KnownGoodInventory {
            entries: vec![tier_one_sha1_entry(
                KnownGoodRoot::Assets,
                "objects/aa/exact",
                KnownGoodArtifactKind::AssetObject,
                MAX_TIER2_ARTIFACT_BYTES,
            )],
            standalone_leaf_repair_sources: BTreeMap::new(),
        };
        assert_eq!(
            exact_file
                .tier2_projection()
                .expect("exact file bound")
                .expected_content_byte_count(),
            MAX_TIER2_ARTIFACT_BYTES
        );
        let oversized = MAX_TIER2_ARTIFACT_BYTES + 1;
        assert_eq!(
            KnownGoodInventory {
                entries: vec![tier_one_sha1_entry(
                    KnownGoodRoot::Assets,
                    "objects/aa/oversized",
                    KnownGoodArtifactKind::AssetObject,
                    oversized,
                )],
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .tier2_projection()
            .expect_err("file byte bound must be closed"),
            Tier2ProjectionError::ArtifactByteLimitExceeded {
                entry_count: 1,
                inventory_ordinal: 0,
                expected_byte_count: oversized,
            }
        );

        let entry_count = usize::try_from(MAX_TIER2_AGGREGATE_BYTES / MAX_TIER2_ARTIFACT_BYTES)
            .expect("bounded entry count");
        let entries = (0..entry_count)
            .map(|ordinal| {
                tier_one_sha1_entry(
                    KnownGoodRoot::Libraries,
                    &format!("aggregate/library-{ordinal}.jar"),
                    KnownGoodArtifactKind::Library,
                    MAX_TIER2_ARTIFACT_BYTES,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            KnownGoodInventory {
                entries: entries.clone(),
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .tier2_projection()
            .expect("exact aggregate bound")
            .expected_content_byte_count(),
            MAX_TIER2_AGGREGATE_BYTES
        );
        let mut above = entries;
        above.push(tier_one_sha1_entry(
            KnownGoodRoot::Libraries,
            "aggregate/overflow.jar",
            KnownGoodArtifactKind::Library,
            1,
        ));
        assert_eq!(
            KnownGoodInventory {
                entries: above,
                standalone_leaf_repair_sources: BTreeMap::new(),
            }
            .tier2_projection()
            .expect_err("aggregate byte bound must be closed"),
            Tier2ProjectionError::AggregateByteLimitExceeded {
                entry_count: entry_count + 1,
                expected_byte_count: MAX_TIER2_AGGREGATE_BYTES + 1,
            }
        );
    }

    #[test]
    fn shuffled_metadata_derives_identical_sorted_inventory() {
        let mut left = fixture(false);
        let mut right = fixture(true);
        let extra = checksum_library(
            "com.mojang:also-strict:1.0",
            "com/mojang/also-strict/1.0/also-strict-1.0.jar",
            SHA_B,
            20,
        );
        left.version.libraries.push(extra.clone());
        right.version.libraries.push(extra);
        right.version.libraries.reverse();
        left.library_authority = fixture_library_authority(&left.version, &left.environment)
            .expect("left library authority");
        right.library_authority = fixture_library_authority(&right.version, &right.environment)
            .expect("right library authority");
        let left = left.derive().unwrap();
        let right = right.derive().unwrap();

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
                    size: 10,
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
                    size: 10,
                },
            })
            .expect_err("conflicting contract");
        assert!(matches!(error, KnownGoodInventoryError::ConflictingEntry));
    }

    #[test]
    fn vanilla_inventory_binds_exact_standalone_library_and_asset_sources() {
        let inventory = fixture(false).derive().expect("vanilla inventory");
        let library_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.path().as_str() == "com/mojang/strict/1.0/strict-1.0.jar")
            .expect("library ordinal");
        let library = inventory
            .bind_standalone_leaf_repair_source(library_ordinal)
            .expect("standalone library source");
        assert_eq!(library.inventory_ordinal(), library_ordinal);
        assert_eq!(library.root(), &KnownGoodRoot::Libraries);
        assert_eq!(library.kind(), KnownGoodArtifactKind::Library);
        assert_eq!(
            library.path().as_str(),
            "com/mojang/strict/1.0/strict-1.0.jar"
        );
        assert_eq!(library.sha1().as_str(), SHA_A);
        assert_eq!(library.size(), 10);
        assert_eq!(library.provider_url(), "https://example.invalid/library");

        let asset_index_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.kind() == KnownGoodArtifactKind::AssetIndex)
            .expect("asset index ordinal");
        let asset_index = inventory
            .bind_standalone_leaf_repair_source(asset_index_ordinal)
            .expect("standalone asset index source");
        assert_eq!(asset_index.root(), &KnownGoodRoot::Assets);
        assert_eq!(asset_index.kind(), KnownGoodArtifactKind::AssetIndex);
        assert_eq!(asset_index.path().as_str(), "indexes/fixture-assets.json");
        assert_eq!(asset_index.provider_url(), "https://example.invalid/assets");

        let asset_object_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.kind() == KnownGoodArtifactKind::AssetObject)
            .expect("asset object ordinal");
        let asset_object = inventory
            .bind_standalone_leaf_repair_source(asset_object_ordinal)
            .expect("standalone asset object source");
        assert_eq!(asset_object.root(), &KnownGoodRoot::Assets);
        assert_eq!(asset_object.kind(), KnownGoodArtifactKind::AssetObject);
        assert_eq!(asset_object.sha1().as_str(), SHA_A);
        assert_eq!(asset_object.size(), 5);
        assert_eq!(
            asset_object.provider_url(),
            format!("{ASSET_OBJECT_BASE_URL}/aa/{SHA_A}")
        );

        for (inventory_ordinal, entry) in inventory.entries().iter().enumerate() {
            if matches!(
                entry.kind(),
                KnownGoodArtifactKind::ClientJar
                    | KnownGoodArtifactKind::VersionMetadata
                    | KnownGoodArtifactKind::LogConfig
                    | KnownGoodArtifactKind::RuntimeManifestProof
                    | KnownGoodArtifactKind::RuntimeReadyMarker
                    | KnownGoodArtifactKind::RuntimeFile
                    | KnownGoodArtifactKind::RuntimeExecutable
                    | KnownGoodArtifactKind::RuntimeDirectory
                    | KnownGoodArtifactKind::RuntimeLink
            ) {
                assert!(matches!(
                    inventory.bind_standalone_leaf_repair_source(inventory_ordinal),
                    Err(KnownGoodRepairSourceError::UnsupportedInventoryEntry)
                ));
            }
        }
        assert!(matches!(
            inventory.bind_standalone_leaf_repair_source(inventory.entries().len()),
            Err(KnownGoodRepairSourceError::UnknownInventoryOrdinal)
        ));
    }

    #[test]
    fn profile_receipt_preserves_authenticated_leaf_sources() {
        let (base, record, declarations, resolved, version_bytes) = profile_receipt_fixture();
        let inventory = KnownGoodInstallReceipt::from_verified_profile_source(
            &base,
            &record,
            resolved,
            &version_bytes,
            declarations,
        )
        .expect("profile receipt")
        .seal_after_version_bundle_commit()
        .into_activation_source()
        .into_parts()
        .1;
        let profile_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.path().as_str() == "example/profile/1/profile-1.jar")
            .expect("profile library ordinal");
        let profile = inventory
            .bind_standalone_leaf_repair_source(profile_ordinal)
            .expect("profile library source");
        assert_eq!(profile.kind(), KnownGoodArtifactKind::Library);
        assert_eq!(profile.provider_url(), "https://example.invalid/library");
        for kind in [
            KnownGoodArtifactKind::AssetIndex,
            KnownGoodArtifactKind::AssetObject,
        ] {
            let inventory_ordinal = inventory
                .entries()
                .iter()
                .position(|entry| entry.kind() == kind)
                .expect("inherited asset ordinal");
            assert_eq!(
                inventory
                    .bind_standalone_leaf_repair_source(inventory_ordinal)
                    .expect("inherited asset source")
                    .kind(),
                kind
            );
        }
        let client_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.kind() == KnownGoodArtifactKind::ClientJar)
            .expect("profile client ordinal");
        assert!(matches!(
            inventory.bind_standalone_leaf_repair_source(client_ordinal),
            Err(KnownGoodRepairSourceError::UnsupportedInventoryEntry)
        ));
    }

    #[test]
    fn zero_byte_asset_object_retains_its_canonical_hash_source() {
        let mut fixture = fixture(false);
        let empty_digest = sha1_digest(&[]);
        fixture.replace_asset_index(
            format!(
                r#"{{"objects":{{"empty":{{"hash":"{}","size":0}}}}}}"#,
                empty_digest.as_str()
            )
            .into_bytes(),
            0,
        );

        let inventory = fixture.derive().expect("zero-byte asset inventory");
        let inventory_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.kind() == KnownGoodArtifactKind::AssetObject)
            .expect("zero-byte object ordinal");
        let source = inventory
            .bind_standalone_leaf_repair_source(inventory_ordinal)
            .expect("zero-byte object source");
        assert_eq!(source.size(), 0);
        assert_eq!(source.sha1(), &empty_digest);
        assert_eq!(source.path().as_str(), asset_object_path(&empty_digest));
        assert_eq!(
            source.provider_url(),
            asset_object_provider_url(&empty_digest)
        );
    }

    #[test]
    fn invalid_asset_object_hash_fails_before_source_authority_exists() {
        let mut fixture = fixture(false);
        fixture.replace_asset_index(
            br#"{"objects":{"invalid":{"hash":"not-a-sha1","size":0}}}"#.to_vec(),
            0,
        );

        assert_eq!(
            fixture.derive(),
            Err(KnownGoodInventoryError::InvalidAssetObject)
        );
    }

    #[test]
    fn inherited_library_plan_url_cannot_mint_repair_authority() {
        let library = checksum_library(
            "com.mojang:inherited:1",
            "com/mojang/inherited/1/inherited-1.jar",
            SHA_A,
            10,
        );
        let mut base = InventoryBuilder::default();
        base.insert(KnownGoodEntry {
            root: KnownGoodRoot::Libraries,
            path: KnownGoodRelativePath::new("com/mojang/inherited/1/inherited-1.jar")
                .expect("library path"),
            kind: KnownGoodArtifactKind::Library,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                size: 10,
            },
        })
        .expect("base library");
        let base = base.finish();
        let mut inherited = InventoryBuilder::default();
        add_exact_inherited_libraries(
            &mut inherited,
            &[library],
            &crate::rules::default_environment(),
            &base,
        )
        .expect("inherited library");
        let inherited = inherited.finish();
        assert!(matches!(
            inherited.bind_standalone_leaf_repair_source(0),
            Err(KnownGoodRepairSourceError::UnsupportedInventoryEntry)
        ));
    }

    #[test]
    fn plain_asset_entries_cannot_mint_repair_authority() {
        let digest = Sha1Digest::from_metadata(SHA_A).expect("asset digest");
        let mut source_free_asset = InventoryBuilder::default();
        source_free_asset
            .insert(KnownGoodEntry {
                root: KnownGoodRoot::Assets,
                path: KnownGoodRelativePath::new(&asset_object_path(&digest))
                    .expect("asset object path"),
                kind: KnownGoodArtifactKind::AssetObject,
                integrity: KnownGoodIntegrity::Sha1 { digest, size: 0 },
            })
            .expect("source-free asset");
        assert!(matches!(
            source_free_asset
                .finish()
                .bind_standalone_leaf_repair_source(0),
            Err(KnownGoodRepairSourceError::UnsupportedInventoryEntry)
        ));
    }

    #[test]
    fn repair_source_contracts_fail_closed_on_invalid_conflicting_and_mismatched_facts() {
        let entry = || KnownGoodEntry {
            root: KnownGoodRoot::Libraries,
            path: KnownGoodRelativePath::new("example/library.jar").expect("library path"),
            kind: KnownGoodArtifactKind::Library,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("digest"),
                size: 10,
            },
        };
        let mut builder = InventoryBuilder::default();
        builder
            .insert_with_standalone_leaf_repair_source(
                entry(),
                Some("https://example.invalid/library.jar"),
            )
            .expect("first source");
        assert_eq!(
            builder.insert_with_standalone_leaf_repair_source(
                entry(),
                Some("https://mirror.invalid/library.jar"),
            ),
            Err(KnownGoodInventoryError::ConflictingRepairSource)
        );

        let mut inventory = builder.finish();
        inventory
            .standalone_leaf_repair_sources
            .get_mut(&0)
            .expect("source contract")
            .digest = Sha1Digest::from_metadata(SHA_B).expect("mismatched digest");
        assert!(matches!(
            inventory.bind_standalone_leaf_repair_source(0),
            Err(KnownGoodRepairSourceError::ContractMismatch)
        ));

        let mut invalid = InventoryBuilder::default();
        assert_eq!(
            invalid.insert_with_standalone_leaf_repair_source(
                entry(),
                Some("file:///tmp/library.jar"),
            ),
            Err(KnownGoodInventoryError::InvalidRepairSource)
        );
        let mut client = entry();
        client.root = KnownGoodRoot::Versions;
        client.kind = KnownGoodArtifactKind::ClientJar;
        assert_eq!(
            invalid.insert_with_standalone_leaf_repair_source(
                client,
                Some("https://example.invalid/client.jar"),
            ),
            Err(KnownGoodInventoryError::InvalidRepairSource)
        );

        let asset_object = |path: &str, digest: &str| KnownGoodEntry {
            root: KnownGoodRoot::Assets,
            path: KnownGoodRelativePath::new(path).expect("asset object path"),
            kind: KnownGoodArtifactKind::AssetObject,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(digest).expect("asset object digest"),
                size: 0,
            },
        };
        assert_eq!(
            invalid.insert_with_standalone_leaf_repair_source(
                asset_object(&format!("objects/aa/{SHA_A}"), SHA_A),
                Some("https://mirror.invalid/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ),
            Err(KnownGoodInventoryError::InvalidRepairSource)
        );
        assert_eq!(
            invalid.insert_with_standalone_leaf_repair_source(
                asset_object(&format!("objects/bb/{SHA_B}"), SHA_A),
                Some(&format!("{ASSET_OBJECT_BASE_URL}/aa/{SHA_A}")),
            ),
            Err(KnownGoodInventoryError::InvalidRepairSource)
        );
        let asset_index = KnownGoodEntry {
            root: KnownGoodRoot::Assets,
            path: KnownGoodRelativePath::new("indexes/invalid/nested.json")
                .expect("safe but noncanonical index path"),
            kind: KnownGoodArtifactKind::AssetIndex,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("asset index digest"),
                size: 1,
            },
        };
        assert_eq!(
            invalid.insert_with_standalone_leaf_repair_source(
                asset_index,
                Some("https://example.invalid/index.json"),
            ),
            Err(KnownGoodInventoryError::InvalidRepairSource)
        );
        let canonical_asset_index = KnownGoodEntry {
            root: KnownGoodRoot::Assets,
            path: KnownGoodRelativePath::new("indexes/canonical.json")
                .expect("canonical index path"),
            kind: KnownGoodArtifactKind::AssetIndex,
            integrity: KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_A).expect("asset index digest"),
                size: 1,
            },
        };
        assert_eq!(
            invalid.insert_with_standalone_leaf_repair_source(
                canonical_asset_index,
                Some("file:///tmp/index.json"),
            ),
            Err(KnownGoodInventoryError::InvalidRepairSource)
        );

        let mut asset_contract = InventoryBuilder::default();
        asset_contract
            .insert_with_standalone_leaf_repair_source(
                asset_object(&format!("objects/aa/{SHA_A}"), SHA_A),
                Some(&format!("{ASSET_OBJECT_BASE_URL}/aa/{SHA_A}")),
            )
            .expect("canonical asset source");
        let mut asset_contract = asset_contract.finish();
        asset_contract
            .standalone_leaf_repair_sources
            .get_mut(&0)
            .expect("asset source contract")
            .provider_url = format!("{ASSET_OBJECT_BASE_URL}/bb/{SHA_B}");
        assert!(matches!(
            asset_contract.bind_standalone_leaf_repair_source(0),
            Err(KnownGoodRepairSourceError::ContractMismatch)
        ));
    }

    #[test]
    fn runtime_projection_replacement_preserves_retained_leaf_sources() {
        let active = fixture(false).derive().expect("active inventory");
        let original_ordinal = active
            .entries()
            .iter()
            .position(|entry| entry.path().as_str() == "com/mojang/strict/1.0/strict-1.0.jar")
            .expect("library ordinal");
        let original_provider = active
            .bind_standalone_leaf_repair_source(original_ordinal)
            .expect("active source")
            .provider_url()
            .to_string();
        let mut runtime = InventoryBuilder::default();
        runtime
            .insert(KnownGoodEntry {
                root: runtime_root(),
                path: KnownGoodRelativePath::new("bin/java").expect("runtime path"),
                kind: KnownGoodArtifactKind::RuntimeExecutable,
                integrity: KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata(SHA_C).expect("runtime digest"),
                    size: 30,
                },
            })
            .expect("replacement runtime");
        let replaced = replace_runtime_projection(
            &active,
            runtime.finish(),
            &RuntimeId::from("java-runtime-delta"),
        )
        .expect("runtime projection replacement");
        let replaced_ordinal = replaced
            .entries()
            .iter()
            .position(|entry| entry.path().as_str() == "com/mojang/strict/1.0/strict-1.0.jar")
            .expect("retained library ordinal");
        assert_eq!(
            replaced
                .bind_standalone_leaf_repair_source(replaced_ordinal)
                .expect("retained source")
                .provider_url(),
            original_provider
        );
    }

    #[test]
    fn runtime_inventory_uses_raw_integrity_not_lzma_transport_integrity() {
        let fixture = fixture(false);
        let inventory = fixture.derive().unwrap();
        let runtime = inventory
            .entries()
            .iter()
            .find(|entry| entry.kind() == KnownGoodArtifactKind::RuntimeExecutable)
            .expect("runtime executable");
        let KnownGoodIntegrity::Sha1 { digest, size } = runtime.integrity() else {
            panic!("runtime executable must have raw integrity")
        };
        assert_eq!(digest.as_str(), SHA_B);
        assert_eq!(*size, 20);
    }

    #[test]
    fn runtime_path_tree_allows_directory_ancestor() {
        let mut fixture = fixture(false);
        let java_path = crate::runtime::runtime_java_relative_path();
        let java_root = java_path.split('/').next().expect("java path root");
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_directory_entry(java_root),
            runtime_file_entry(java_path),
        ]));

        let inventory = fixture.derive().expect("valid runtime tree");

        assert_entry(
            &inventory,
            &runtime_root(),
            java_root,
            KnownGoodArtifactKind::RuntimeDirectory,
            &KnownGoodIntegrity::Directory,
        );
        assert!(has_kind(
            &inventory,
            KnownGoodArtifactKind::RuntimeExecutable
        ));
    }

    #[test]
    fn runtime_path_tree_rejects_file_ancestor() {
        let mut fixture = fixture(false);
        let java_path = crate::runtime::runtime_java_relative_path();
        let java_root = java_path.split('/').next().expect("java path root");
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_file_entry(java_root),
            runtime_file_entry(java_path),
        ]));

        let error = fixture.derive().expect_err("file ancestor");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
    }

    #[test]
    fn runtime_path_tree_rejects_link_ancestor() {
        let mut fixture = fixture(false);
        let java_path = crate::runtime::runtime_java_relative_path();
        let java_root = java_path.split('/').next().expect("java path root");
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_link_entry(java_root, "java"),
            runtime_file_entry(java_path),
        ]));

        let error = fixture.derive().expect_err("link ancestor");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
    }

    #[test]
    fn runtime_path_tree_rejects_incompatible_exact_path() {
        let mut fixture = fixture(false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_file_entry(crate::runtime::runtime_java_relative_path()),
            runtime_file_entry(".axial-ready"),
        ]));

        let error = fixture.derive().expect_err("reserved path");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
    }

    #[test]
    fn platform_java_link_is_tier_zero_executable_or_rejected_when_unsupported() {
        let mut fixture = fixture(false);
        let java_path = crate::runtime::runtime_java_relative_path();
        let java_parent = Path::new(java_path).parent().expect("java parent");
        let target_path = java_parent.join("java-real");
        let target_path = target_path.to_str().expect("UTF-8 java target");
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_file_entry(target_path),
            runtime_link_entry(java_path, "java-real"),
        ]));

        if cfg!(target_os = "windows") {
            assert_eq!(
                fixture.derive().expect_err("Windows Java link"),
                KnownGoodInventoryError::UnsupportedRuntimeEntry
            );
            return;
        }

        let inventory = fixture.derive().expect("platform Java link inventory");
        assert_entry(
            &inventory,
            &runtime_root(),
            java_path,
            KnownGoodArtifactKind::RuntimeExecutable,
            &KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget("java-real".to_string())),
        );
    }

    #[test]
    fn asset_objects_deduplicate_by_content_address() {
        let fixture = fixture(false);
        let inventory = fixture.derive().unwrap();
        let objects = inventory
            .entries()
            .iter()
            .filter(|entry| entry.kind() == KnownGoodArtifactKind::AssetObject)
            .collect::<Vec<_>>();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].path().as_str(), format!("objects/aa/{SHA_A}"));
    }

    #[test]
    fn vanilla_missing_library_checksum_uses_fresh_streamed_identity() {
        let mut fixture = fixture(false);
        fixture
            .version
            .libraries
            .push(checksumless_loader_library());
        fixture.library_authority =
            fixture_library_authority(&fixture.version, &fixture.environment)
                .expect("streamed checksumless declaration");

        let inventory = fixture.derive().expect("streamed inventory");
        let bytes = vec![b'x'; 12];
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            "net/loader/loader-unverified/1.0/loader-unverified-1.0.jar",
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: sha1_digest(&bytes),
                size: bytes.len() as u64,
            },
        );
    }

    #[test]
    fn vanilla_version_identity_mismatch_fails_closed() {
        let mut fixture = fixture(false);
        fixture.version_manifest.id = "different-version".to_string();

        let error = fixture.derive().expect_err("version mismatch");
        assert_eq!(error, KnownGoodInventoryError::VersionIdentityMismatch);
    }

    #[test]
    fn runtime_component_identity_mismatch_fails_closed() {
        let mut fixture = fixture(false);
        fixture.runtime_id = RuntimeId::from("java-runtime-gamma");

        let error = fixture.derive().expect_err("runtime mismatch");
        assert_eq!(error, KnownGoodInventoryError::RuntimeIdentityMismatch);
    }

    #[test]
    fn unsafe_library_plan_maps_to_closed_inventory_error() {
        let mut fixture = fixture(false);
        fixture.version.libraries.push(checksum_library(
            "net.example:unsafe:1.0",
            "../outside.jar",
            SHA_A,
            10,
        ));

        let error = fixture.derive().expect_err("unsafe plan");
        assert_eq!(error, KnownGoodInventoryError::InvalidLibraryPlan);
    }

    #[test]
    fn conflicting_library_plan_maps_to_closed_inventory_error() {
        let mut fixture = fixture(false);
        fixture.version.libraries.push(checksum_library(
            "net.example:conflict:1.0",
            "com/mojang/strict/1.0/strict-1.0.jar",
            SHA_B,
            11,
        ));

        let error = fixture.derive().expect_err("conflicting plan");
        assert_eq!(error, KnownGoodInventoryError::InvalidLibraryPlan);
    }

    #[test]
    fn runtime_manifest_proof_is_canonical_across_provider_object_order() {
        let left = fixture(false);
        let right = fixture(true);
        let left = left.derive().unwrap();
        let right = right.derive().unwrap();
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
        let fixture = fixture(false);
        let inventory = fixture.derive().unwrap();
        let link = entry(&inventory, &runtime_root(), "java-link");

        assert_eq!(link.kind(), KnownGoodArtifactKind::RuntimeLink);
        let KnownGoodIntegrity::LinkTarget(target) = link.integrity() else {
            panic!("runtime link must carry its canonical target")
        };
        assert_eq!(
            target.as_str(),
            crate::runtime::runtime_java_relative_path()
        );
    }

    #[test]
    fn oversized_runtime_manifest_is_rejected_before_parsing() {
        let mut fixture = fixture(false);
        fixture.runtime_manifest_bytes = vec![b' '; MAX_KNOWN_GOOD_RUNTIME_MANIFEST_BYTES + 1];
        fixture.runtime_manifest_expected = expected_for(&fixture.runtime_manifest_bytes);

        let error = fixture.derive().expect_err("oversized manifest");
        assert_eq!(error, KnownGoodInventoryError::InputTooLarge);
    }

    struct Fixture {
        version: VersionJson,
        version_manifest: ManifestEntry,
        version_metadata_size: u64,
        library_authority: SealedExactLibraryDeclarations,
        asset_index: Vec<u8>,
        runtime_manifest_bytes: Vec<u8>,
        runtime_manifest_expected: ExpectedIntegrity,
        runtime_id: RuntimeId,
        environment: Environment,
    }

    impl Fixture {
        fn derive(&self) -> Result<KnownGoodInventory, KnownGoodInventoryError> {
            self.derive_version(&self.version)
        }

        fn derive_version(
            &self,
            version: &VersionJson,
        ) -> Result<KnownGoodInventory, KnownGoodInventoryError> {
            let version_expected = ExpectedIntegrity::from_sha1(&self.version_manifest.sha1);
            derive_known_good_inventory(
                version,
                &self.library_authority,
                Some(&self.asset_index),
                VersionSourceObservation {
                    identity: &self.version_manifest.id,
                    expected: &version_expected,
                    metadata_size: self.version_metadata_size,
                },
                Some(RuntimeSourceObservation {
                    component: &self.runtime_id,
                    manifest_bytes: &self.runtime_manifest_bytes,
                    manifest_expected: self.runtime_manifest_expected.clone(),
                }),
                &self.environment,
            )
        }

        fn replace_runtime_manifest(&mut self, bytes: Vec<u8>) {
            self.runtime_manifest_expected = expected_for(&bytes);
            self.runtime_manifest_bytes = bytes;
        }

        fn replace_asset_index(&mut self, bytes: Vec<u8>, total_size: i64) {
            self.version.asset_index.sha1 = sha1_digest(&bytes).as_str().to_string();
            self.version.asset_index.size = bytes.len() as i64;
            self.version.asset_index.total_size = total_size;
            self.asset_index = bytes;
            self.library_authority = fixture_library_authority(&self.version, &self.environment)
                .expect("replacement asset index library authority");
        }
    }

    fn fixture(shuffled: bool) -> Fixture {
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
        if shuffled {
            libraries.reverse();
        }
        let version_id = "fixture-version".to_string();
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
        let environment = Environment {
            os_name: "linux".to_string(),
            os_arch: "x86_64".to_string(),
            os_version: String::new(),
            features: HashMap::new(),
        };
        let library_authority = fixture_library_authority(&version, &environment)
            .expect("fixture library declarations");
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
            version_metadata_size: 128,
            library_authority,
            asset_index,
            runtime_manifest_bytes,
            runtime_manifest_expected,
            runtime_id: RuntimeId::from("java-runtime-delta"),
            environment,
        }
    }

    fn fixture_library_authority(
        version: &VersionJson,
        environment: &Environment,
    ) -> Result<SealedExactLibraryDeclarations, KnownGoodInventoryError> {
        let plans = library_artifact_plans_for(&version.libraries, environment)
            .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
        let streamed = plans
            .into_iter()
            .filter(|plan| plan.expected.size.is_none() || plan.expected.sha1.is_none())
            .map(|plan| {
                let bytes = match plan.expected.size {
                    Some(size) => vec![b'x'; usize::try_from(size).expect("fixture size")],
                    None => format!(
                        "authenticated fixture bytes for {}",
                        plan.relative_path.as_str()
                    )
                    .into_bytes(),
                };
                let sha1: [u8; 20] = Sha1::digest(&bytes).into();
                ExactLibraryDownloadProof::new_bound_for_test(
                    plan.relative_path,
                    plan.is_native,
                    plan.source_url.expect("incomplete fixture source URL"),
                    plan.expected,
                    bytes.len() as u64,
                    sha1,
                )
            })
            .collect();
        seal_vanilla_library_declarations_for_test(version, environment, streamed)
            .map_err(|_| KnownGoodInventoryError::VanillaLibraryProofMismatch)
    }

    fn loader_record(component: LoaderComponentId) -> LoaderBuildRecord {
        let (strategy, artifact_kind, install_source) = match component {
            LoaderComponentId::Fabric | LoaderComponentId::Quilt => (
                if component == LoaderComponentId::Fabric {
                    LoaderInstallStrategy::FabricProfile
                } else {
                    LoaderInstallStrategy::QuiltProfile
                },
                LoaderArtifactKind::ProfileJson,
                LoaderInstallSource::ProfileJson {
                    url: "https://example.invalid/profile".to_string(),
                },
            ),
            LoaderComponentId::Forge | LoaderComponentId::NeoForge => (
                if component == LoaderComponentId::Forge {
                    LoaderInstallStrategy::ForgeModern
                } else {
                    LoaderInstallStrategy::NeoForgeModern
                },
                LoaderArtifactKind::InstallerJar,
                LoaderInstallSource::InstallerJar {
                    url: "https://example.invalid/installer".to_string(),
                },
            ),
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

    fn test_base_client_inventory(version: &VersionJson) -> KnownGoodInventory {
        let client = version.downloads.client.as_ref().expect("base client");
        let mut builder = InventoryBuilder::default();
        builder
            .insert(KnownGoodEntry {
                root: KnownGoodRoot::Versions,
                path: KnownGoodRelativePath::new(&format!("{0}/{0}.jar", version.id))
                    .expect("base client path"),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: KnownGoodIntegrity::Sha1 {
                    digest: Sha1Digest::from_metadata(&client.sha1).expect("base client digest"),
                    size: u64::try_from(client.size).expect("base client size"),
                },
            })
            .expect("base client entry");
        builder.finish()
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
        let java_path = crate::runtime::runtime_java_relative_path();
        let executable = runtime_executable_entry(java_path);
        let directory = runtime_directory_entry("bin");
        let link = runtime_link_entry("java-link", &format!("./{java_path}"));
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
            crate::runtime::runtime_java_relative_path(),
            KnownGoodArtifactKind::RuntimeExecutable,
            &KnownGoodIntegrity::Sha1 {
                digest: Sha1Digest::from_metadata(SHA_B).unwrap(),
                size: 20,
            },
        );
        assert_entry(
            inventory,
            &root,
            "java-link",
            KnownGoodArtifactKind::RuntimeLink,
            &KnownGoodIntegrity::LinkTarget(KnownGoodLinkTarget(
                crate::runtime::runtime_java_relative_path().to_string(),
            )),
        );
    }

    fn has_kind(inventory: &KnownGoodInventory, kind: KnownGoodArtifactKind) -> bool {
        inventory.entries().iter().any(|entry| entry.kind() == kind)
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

    #[test]
    fn activation_source_is_the_only_public_receipt_inventory_transition() {
        let source = include_str!("known_good.rs");
        assert!(!source.contains(concat!("pub fn into_", "inventory")));
        assert!(!source.contains(concat!("pub fn derive_known_good_", "inventory")));
        assert!(!source.contains(concat!("KnownGoodInventory", "Input")));
        assert!(!source.contains(concat!("KnownGoodVanilla", "Source")));
        assert!(!source.contains(concat!("RuntimeInventory", "Input")));
        assert!(!source.contains(concat!("from_verified_vanilla_", "source")));
        assert!(!source.contains(concat!("pub struct KnownGoodInstall", "Shape")));
        for receipt in [
            "pub struct KnownGoodInstallReceipt",
            "pub struct KnownGoodReconstructionReceipt",
        ] {
            let position = source.find(receipt).expect("receipt declaration");
            let derive = source[..position]
                .rsplit("#[derive(")
                .next()
                .and_then(|tail| tail.split(")]").next())
                .expect("receipt derive");
            assert!(!derive.contains("Clone"));
            assert!(!derive.contains("Serialize"));
            assert!(!derive.contains("Deserialize"));
            if receipt == "pub struct KnownGoodReconstructionReceipt" {
                assert!(!derive.contains("Debug"));
            }
        }
        let reconstruction_impl = source
            .split("impl KnownGoodReconstructionReceipt")
            .nth(1)
            .and_then(|tail| {
                tail.split("pub(crate) fn reconstructed_effective_version")
                    .next()
            })
            .expect("reconstruction receipt implementation");
        assert!(!reconstruction_impl.contains("into_parts"));
        assert!(!reconstruction_impl.contains("from_"));
    }
}
