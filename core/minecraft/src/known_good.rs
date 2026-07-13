use crate::artifact_path::{
    ArtifactRelativePath, MAX_ARTIFACT_PATH_SEGMENT_BYTES, MAX_ARTIFACT_RELATIVE_PATH_BYTES,
};
use crate::download::{
    AuthenticatedSelectedArtifactSource, CompletedVanillaInstallAuthority, ExpectedIntegrity,
    LibraryArtifactPlan, PendingVanillaInstallSourceAuthority, ReconstructedVanillaAuthority,
    SelectedDownloadArtifactKind, library_artifact_plans_for, parse_asset_index,
};
use crate::known_good_libraries::{
    PendingInstallerPublications, RetainedInstallerLibrarySource, SealedExactLibraryDeclarations,
    SealedLibraryKind,
};
use crate::launch::{Library, VersionJson, effective_java_version_for};
use crate::loaders::{
    AuthenticatedInstallerReceiptInput, AuthenticatedInstallerReconstructionAuthority,
    AuthenticatedLegacyOverlayAuthority, LoaderBuildRecord, LoaderComponentId, LoaderInstallSource,
    LoaderInstallStrategy, VerifiedInstallerClientBytes, VerifiedInstallerReceiptSource,
    compose_loader_version,
};
#[cfg(test)]
use crate::manifest::ManifestEntry;
use crate::rules::Environment;
use crate::runtime::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, RuntimeId, RuntimeSourceReceipt,
    component_manifest_proof_bytes, plan_runtime_manifest_files, preferred_runtime_component,
};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

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
    RuntimeManifestProof,
    RuntimeReadyMarker,
    RuntimeFile,
    RuntimeExecutable,
    RuntimeDirectory,
    RuntimeLink,
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

#[derive(Debug, Eq, PartialEq)]
pub struct KnownGoodInventory {
    entries: Vec<KnownGoodEntry>,
}

impl KnownGoodInventory {
    pub fn entries(&self) -> &[KnownGoodEntry] {
        &self.entries
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct KnownGoodInstallReceipt {
    authenticated: AuthenticatedKnownGoodReceipt,
}

#[derive(Eq, PartialEq)]
pub struct KnownGoodReconstructionReceipt {
    authenticated: AuthenticatedKnownGoodReceipt,
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

pub(crate) struct PendingVanillaInstallReceipt {
    authenticated: AuthenticatedKnownGoodReceipt,
}

pub(crate) struct PendingInstallerReceipt {
    receipt: KnownGoodInstallReceipt,
    publications: PendingInstallerPublications,
}

pub(crate) struct PendingInstallerReceiptPublication {
    receipt: KnownGoodInstallReceipt,
    publications: PendingInstallerPublications,
}

impl PendingInstallerReceipt {
    pub(crate) fn into_publications(
        self,
    ) -> (
        PendingInstallerReceiptPublication,
        Vec<RetainedInstallerLibrarySource>,
    ) {
        let (publications, sources) = self.publications.into_sources();
        (
            PendingInstallerReceiptPublication {
                receipt: self.receipt,
                publications,
            },
            sources,
        )
    }
}

impl PendingInstallerReceiptPublication {
    pub(crate) fn complete(
        self,
        materialized: Vec<crate::download::MaterializedLibraryIdentity>,
    ) -> Result<KnownGoodInstallReceipt, KnownGoodInventoryError> {
        self.publications
            .complete(materialized)
            .map_err(|_| KnownGoodInventoryError::InstallerLibraryProofMismatch)?;
        Ok(self.receipt)
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

    pub(crate) fn from_verified_legacy_archive_source(
        base: &Self,
        record: &LoaderBuildRecord,
        resolved_version: VersionJson,
        version_bytes: &[u8],
        child_client_bytes: &[u8],
    ) -> Result<Self, KnownGoodInventoryError> {
        Ok(Self {
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
    ) -> Result<Self, KnownGoodInventoryError> {
        Ok(Self {
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
        let (source, library_declarations, pending_publications) = input.into_parts();
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
            receipt: Self { authenticated },
            publications: pending_publications,
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
        let integrity = if let Some((sealed_kind, sha1, size)) =
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
            KnownGoodIntegrity::Sha1 {
                digest: sha1_array_digest(&sha1),
                size,
            }
        } else if allows_base {
            matching_base_library_integrity(&base, &plan, &path, kind)
                .map_err(|_| KnownGoodInventoryError::InstallerLibraryProofMismatch)?
        } else {
            return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
        };
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Libraries,
            path,
            kind,
            integrity,
        })?;
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
        let integrity = if let Some((sealed_kind, sealed_sha1, sealed_size)) =
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
            KnownGoodIntegrity::Sha1 {
                digest: sha1_array_digest(&sealed_sha1),
                size: sealed_size,
            }
        } else {
            matching_base_library_integrity(base, &plan, &path, kind)?
        };
        builder.insert(KnownGoodEntry {
            root: KnownGoodRoot::Libraries,
            path,
            kind,
            integrity,
        })?;
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
}

pub(crate) fn reconstructed_effective_version(
    receipt: &KnownGoodReconstructionReceipt,
) -> &VersionJson {
    &receipt.authenticated.effective_version
}

pub(crate) fn seal_reconstructed_profile_source(
    base: KnownGoodReconstructionReceipt,
    record: &LoaderBuildRecord,
    resolved_version: VersionJson,
    version_bytes: &[u8],
    library_declarations: SealedExactLibraryDeclarations,
) -> Result<KnownGoodReconstructionReceipt, KnownGoodInventoryError> {
    let authenticated = derive_profile_receipt(
        &base.authenticated,
        record,
        resolved_version,
        version_bytes,
        library_declarations,
    )?;
    Ok(KnownGoodReconstructionReceipt { authenticated })
}

pub(crate) fn seal_reconstructed_installer_source(
    authority: AuthenticatedInstallerReconstructionAuthority,
) -> Result<KnownGoodReconstructionReceipt, KnownGoodInventoryError> {
    let (base, base_client_source, record, input, resolved_version, version_bytes, child_client) =
        authority.consume_for_sealing();
    let (source, library_declarations) = input.into_parts();
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
    Ok(KnownGoodReconstructionReceipt { authenticated })
}

pub(crate) fn seal_reconstructed_legacy_archive_source(
    authority: AuthenticatedLegacyOverlayAuthority,
) -> Result<KnownGoodReconstructionReceipt, KnownGoodInventoryError> {
    let (
        base,
        base_client_source,
        archive_source,
        record,
        resolved_version,
        version_bytes,
        child_client_bytes,
    ) = authority.consume_for_sealing();
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
    Ok(KnownGoodReconstructionReceipt { authenticated })
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
) -> Result<KnownGoodReconstructionReceipt, KnownGoodInventoryError> {
    let (version, environment, libraries, version_source, asset_source, runtime_source) =
        authority.into_parts();
    let authenticated = authenticate_vanilla_authority(
        version,
        environment,
        libraries,
        version_source,
        asset_source,
        runtime_source,
    )?;
    Ok(KnownGoodReconstructionReceipt { authenticated })
}

pub(crate) fn seal_completed_vanilla_install(
    authority: CompletedVanillaInstallAuthority,
) -> KnownGoodInstallReceipt {
    KnownGoodInstallReceipt {
        authenticated: authority.into_pending_receipt().authenticated,
    }
}

pub(crate) fn authenticate_pending_vanilla_install(
    authority: PendingVanillaInstallSourceAuthority,
) -> Result<PendingVanillaInstallReceipt, KnownGoodInventoryError> {
    let (version, environment, libraries, version_source, asset_source, runtime_source) =
        authority.into_parts();
    let authenticated = authenticate_vanilla_authority(
        version,
        environment,
        libraries,
        version_source,
        asset_source,
        runtime_source,
    )?;
    Ok(PendingVanillaInstallReceipt { authenticated })
}

fn authenticate_vanilla_authority(
    resolved_version: VersionJson,
    environment: Environment,
    libraries: SealedExactLibraryDeclarations,
    version_source: AuthenticatedSelectedArtifactSource,
    asset_source: Option<AuthenticatedSelectedArtifactSource>,
    runtime_source: Option<RuntimeSourceReceipt>,
) -> Result<AuthenticatedKnownGoodReceipt, KnownGoodInventoryError> {
    if version_source.kind() != SelectedDownloadArtifactKind::VersionJson
        || version_source.logical_identity() != resolved_version.id
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
    if authenticated != resolved_version {
        return Err(KnownGoodInventoryError::VersionIdentityMismatch);
    }
    let asset_index_bytes = match asset_source.as_ref() {
        Some(source)
            if source.kind() == SelectedDownloadArtifactKind::AssetIndex
                && source.logical_identity() == resolved_version.asset_index.id
                && source.provider_url() == resolved_version.asset_index.url
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
    let runtime_observation = runtime_source
        .as_ref()
        .map(|receipt| RuntimeSourceObservation {
            component: receipt.component(),
            manifest_bytes: receipt.bytes(),
            manifest_expected: ExpectedIntegrity {
                size: Some(receipt.expected_size()),
                sha1: Some(receipt.expected_sha1().to_string()),
            },
        });
    let inventory = derive_known_good_inventory(
        &resolved_version,
        &libraries,
        asset_index_bytes,
        VersionSourceObservation {
            identity: version_source.logical_identity(),
            expected: version_source.expected(),
            metadata_size: version_metadata_size,
        },
        runtime_observation,
        &environment,
    )?;
    Ok(AuthenticatedKnownGoodReceipt {
        version_id,
        inventory,
        effective_version: resolved_version,
        environment,
    })
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
    VanillaLibraryProofMismatch,
    ProfileLibraryProofMismatch,
    InstallerLibraryProofMismatch,
    ConflictingEntry,
    ConflictingRuntimePath,
    TooManyEntries,
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
        let (kind, observed_sha1, size) = authority
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
        builder.insert(KnownGoodEntry {
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
        })?;
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
        builder.insert(entry.clone())?;
    }
    Ok(())
}

fn matching_base_library_integrity(
    base: &AuthenticatedKnownGoodReceipt,
    plan: &LibraryArtifactPlan,
    path: &KnownGoodRelativePath,
    kind: KnownGoodArtifactKind,
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
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
    Ok(entry.integrity.clone())
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
    builder.insert(KnownGoodEntry {
        root: KnownGoodRoot::Assets,
        path: KnownGoodRelativePath::new(&format!("indexes/{}.json", index_id.as_str()))?,
        kind: KnownGoodArtifactKind::AssetIndex,
        integrity: expected_integrity_with_observed_size(&expected, bytes.len() as u64)?,
    })?;

    let index = parse_asset_index(bytes).map_err(|_| KnownGoodInventoryError::AssetIndexParse)?;
    for object in index.objects.values() {
        let digest = Sha1Digest::from_metadata(&object.hash)
            .map_err(|_| KnownGoodInventoryError::InvalidAssetObject)?;
        let size =
            u64::try_from(object.size).map_err(|_| KnownGoodInventoryError::InvalidAssetObject)?;
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
            integrity: expected_integrity(&expected)?,
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
    use std::path::PathBuf;

    fn profile_receipt_fixture() -> (
        KnownGoodInstallReceipt,
        LoaderBuildRecord,
        SealedExactLibraryDeclarations,
        VersionJson,
        Vec<u8>,
    ) {
        let record = loader_record(LoaderComponentId::Fabric);
        let mut base_version = fixture(false).version;
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
        let root = PathBuf::from("/managed/libraries");
        let (libraries, environment) = pending.profile_plan_inputs().expect("profile plan inputs");
        let jobs = library_artifact_plans_for(libraries, environment)
            .expect("profile plans")
            .into_iter()
            .map(|plan| crate::download::DownloadJob {
                relative_path: plan.relative_path.clone(),
                path: plan.relative_path.join_under(&root),
                url: plan.source_url.expect("profile URL"),
                name: plan.name,
                expected: plan.expected,
                is_native: plan.is_native,
            })
            .collect();
        let (pending, classified) = pending
            .classify_jobs(&root, jobs)
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
    fn profile_receipt_binds_authored_recombination_and_exact_base_shadowing() {
        let (base, record, declarations, resolved, version_bytes) = profile_receipt_fixture();
        let inventory = KnownGoodInstallReceipt::from_verified_profile_source(
            &base,
            &record,
            resolved,
            &version_bytes,
            declarations,
        )
        .expect("profile receipt")
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
        let inventory = receipt.into_activation_source().into_parts().1;

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
                ArtifactRelativePath::new("com/mojang/strict/1.0/strict-1.0.jar")
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
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_directory_entry("bin"),
            runtime_file_entry("bin/java"),
        ]));

        let inventory = fixture.derive().expect("valid runtime tree");

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
        let mut fixture = fixture(false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_file_entry("bin"),
            runtime_file_entry("bin/java"),
        ]));

        let error = fixture.derive().expect_err("file ancestor");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
    }

    #[test]
    fn runtime_path_tree_rejects_link_ancestor() {
        let mut fixture = fixture(false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[
            runtime_link_entry("bin", "java"),
            runtime_file_entry("bin/java"),
        ]));

        let error = fixture.derive().expect_err("link ancestor");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
    }

    #[test]
    fn runtime_path_tree_rejects_incompatible_exact_path() {
        let mut fixture = fixture(false);
        fixture.replace_runtime_manifest(runtime_manifest_with_entries(&[runtime_file_entry(
            ".axial-ready",
        )]));

        let error = fixture.derive().expect_err("reserved path");
        assert_eq!(error, KnownGoodInventoryError::ConflictingRuntimePath);
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
        assert_eq!(target.as_str(), "bin/java");
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
                size: 20,
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
