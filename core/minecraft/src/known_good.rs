use crate::artifact_path::{
    ArtifactRelativePath, MAX_ARTIFACT_PATH_SEGMENT_BYTES, MAX_ARTIFACT_RELATIVE_PATH_BYTES,
};
use crate::download::{
    ExactLibraryDownloadProof, ExpectedIntegrity, LibraryArtifactPlan, library_artifact_plans_for,
    parse_asset_index,
};
use crate::launch::{Library, VersionJson, merge_libraries_prefer_first};
use crate::loaders::{
    LoaderBuildRecord, LoaderComponentId, LoaderInstallStrategy, VerifiedInstallerClientBytes,
    VerifiedInstallerReceiptSource, VerifiedProcessorOutputs, compose_loader_version,
    installed_loader_metadata_bytes,
};
use crate::manifest::ManifestEntry;
use crate::rules::Environment;
use crate::runtime::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, RuntimeId, component_manifest_proof_bytes,
    plan_runtime_manifest_files, preferred_runtime_component,
};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
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

#[derive(Debug, Eq, PartialEq)]
pub struct KnownGoodInstallReceipt {
    version_id: KnownGoodId,
    inventory: KnownGoodInventory,
    effective_version: VersionJson,
    environment: Environment,
}

pub(crate) struct VerifiedInstallerLibraryAuthority {
    entries: BTreeMap<ArtifactRelativePath, VerifiedInstallerLibraryFact>,
}

pub(crate) struct VerifiedProfileLibraryAuthority {
    libraries: Vec<Library>,
    entries: BTreeMap<ArtifactRelativePath, VerifiedProfileLibraryFact>,
}

struct VerifiedProfileLibraryFact {
    size: Option<u64>,
    sha1: [u8; 20],
    is_native: bool,
}

pub(crate) fn seal_verified_profile_library_authority(
    libraries: &[Library],
    downloads: Vec<ExactLibraryDownloadProof>,
) -> Result<VerifiedProfileLibraryAuthority, KnownGoodInventoryError> {
    let environment = crate::rules::default_environment();
    let plans = library_artifact_plans_for(libraries, &environment)
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    let expected = plans
        .into_iter()
        .map(|plan| (plan.relative_path.clone(), plan))
        .collect::<BTreeMap<_, _>>();
    let mut entries = BTreeMap::new();
    for download in downloads {
        let (path, size, sha1) = download.into_parts();
        let plan = expected
            .get(&path)
            .ok_or(KnownGoodInventoryError::ProfileLibraryProofMismatch)?;
        if entries
            .insert(
                path,
                VerifiedProfileLibraryFact {
                    size,
                    sha1,
                    is_native: plan.is_native,
                },
            )
            .is_some()
        {
            return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
        }
    }
    if entries.len() != expected.len() {
        return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
    }
    for (path, plan) in &expected {
        let proof = entries
            .get(path)
            .ok_or(KnownGoodInventoryError::ProfileLibraryProofMismatch)?;
        if proof.is_native != plan.is_native
            || plan
                .expected
                .size
                .is_some_and(|size| proof.size != Some(size))
            || plan.expected.sha1.as_deref().is_some_and(|sha1| {
                !Sha1Digest::from_metadata(sha1)
                    .is_ok_and(|expected| expected == sha1_array_digest(&proof.sha1))
            })
        {
            return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
        }
    }

    let mut enriched = libraries.to_vec();
    for library in &mut enriched {
        let selected = library_artifact_plans_for(std::slice::from_ref(library), &environment)
            .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
        if selected.is_empty() {
            continue;
        }
        for plan in &selected {
            let proof = entries
                .get(&plan.relative_path)
                .ok_or(KnownGoodInventoryError::ProfileLibraryProofMismatch)?;
            author_profile_library_integrity(library, plan, proof)?;
        }
    }

    let enriched_plans = library_artifact_plans_for(&enriched, &environment)
        .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    if enriched_plans.len() != expected.len()
        || enriched_plans.iter().any(|plan| {
            let Some(proof) = entries.get(&plan.relative_path) else {
                return true;
            };
            plan.is_native != proof.is_native
                || plan.expected.size != proof.size
                || plan.expected.sha1.as_deref().is_none_or(|sha1| {
                    !Sha1Digest::from_metadata(sha1)
                        .is_ok_and(|digest| digest == sha1_array_digest(&proof.sha1))
                })
        })
    {
        return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
    }

    Ok(VerifiedProfileLibraryAuthority {
        libraries: enriched,
        entries,
    })
}

fn author_profile_library_integrity(
    library: &mut Library,
    plan: &LibraryArtifactPlan,
    proof: &VerifiedProfileLibraryFact,
) -> Result<(), KnownGoodInventoryError> {
    let size = proof
        .size
        .ok_or(KnownGoodInventoryError::ProfileLibraryProofMismatch)?;
    let size = i64::try_from(size).map_err(|_| KnownGoodInventoryError::InputTooLarge)?;
    let digest = sha1_array_digest(&proof.sha1).as_str().to_string();
    if !plan.is_native {
        library.sha1.clone_from(&digest);
        library.size = size;
    }

    if let Some(downloads) = library.downloads.as_mut() {
        let mut matches = 0;
        if let Some(artifact) = downloads.artifact.as_mut()
            && ArtifactRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path)
        {
            artifact.sha1.clone_from(&digest);
            artifact.size = size;
            matches += 1;
        }
        for artifact in downloads.classifiers.values_mut() {
            if ArtifactRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path) {
                artifact.sha1.clone_from(&digest);
                artifact.size = size;
                matches += 1;
            }
        }
        if matches != 1 {
            return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
        }
    } else if plan.is_native {
        return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
    }
    Ok(())
}

impl VerifiedProfileLibraryAuthority {
    pub(crate) fn libraries(&self) -> &[Library] {
        &self.libraries
    }
}

struct VerifiedInstallerLibraryFact {
    size: Option<u64>,
    sha1: [u8; 20],
    terminal_bytes: Option<Arc<[u8]>>,
}

pub(crate) fn seal_verified_installer_library_authority(
    source: &VerifiedInstallerReceiptSource,
    downloads: Vec<ExactLibraryDownloadProof>,
    outputs: VerifiedProcessorOutputs,
) -> Result<VerifiedInstallerLibraryAuthority, KnownGoodInventoryError> {
    let plans =
        library_artifact_plans_for(source.libraries(), &crate::rules::default_environment())
            .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    let expected = plans
        .into_iter()
        .map(|plan| (plan.relative_path.clone(), plan))
        .collect::<BTreeMap<_, _>>();
    let mut entries = BTreeMap::new();

    for download in downloads {
        let (path, size, sha1) = download.into_parts();
        insert_installer_library_fact(
            &mut entries,
            path,
            VerifiedInstallerLibraryFact {
                size,
                sha1,
                terminal_bytes: None,
            },
        )?;
    }
    for artifact in source.embedded_maven_artifacts() {
        if !expected.contains_key(artifact.relative_path()) {
            continue;
        }
        insert_installer_library_fact(
            &mut entries,
            artifact.relative_path().clone(),
            VerifiedInstallerLibraryFact {
                size: Some(artifact.bytes().len() as u64),
                sha1: Sha1::digest(artifact.bytes()).into(),
                terminal_bytes: None,
            },
        )?;
    }
    for (path, output) in outputs.into_entries() {
        let (bytes, size, sha1) = output.into_parts();
        let actual_sha1: [u8; 20] = Sha1::digest(bytes.as_ref()).into();
        if size != bytes.len() as u64 || sha1 != actual_sha1 {
            return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
        }
        insert_installer_library_fact(
            &mut entries,
            path,
            VerifiedInstallerLibraryFact {
                size: Some(size),
                sha1,
                terminal_bytes: Some(bytes),
            },
        )?;
    }

    if entries.len() != expected.len() || entries.keys().any(|path| !expected.contains_key(path)) {
        return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
    }
    for (path, plan) in expected {
        let fact = entries
            .get(&path)
            .ok_or(KnownGoodInventoryError::InstallerLibraryProofMismatch)?;
        if plan
            .expected
            .size
            .is_some_and(|size| fact.size != Some(size))
            || plan.expected.sha1.as_deref().is_some_and(|sha1| {
                !Sha1Digest::from_metadata(sha1)
                    .is_ok_and(|expected| expected == sha1_array_digest(&fact.sha1))
            })
        {
            return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
        }
    }
    Ok(VerifiedInstallerLibraryAuthority { entries })
}

impl VerifiedInstallerLibraryAuthority {
    pub(crate) fn into_terminal_materializations(self) -> Vec<(ArtifactRelativePath, Arc<[u8]>)> {
        self.entries
            .into_iter()
            .filter_map(|(path, fact)| fact.terminal_bytes.map(|bytes| (path, bytes)))
            .collect()
    }
}

fn insert_installer_library_fact(
    entries: &mut BTreeMap<ArtifactRelativePath, VerifiedInstallerLibraryFact>,
    path: ArtifactRelativePath,
    fact: VerifiedInstallerLibraryFact,
) -> Result<(), KnownGoodInventoryError> {
    if entries.insert(path, fact).is_some() {
        return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
    }
    Ok(())
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
        expected_integrity(&expected)?;
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

        let libraries = library_artifact_plans_for(&resolved_version.libraries, &base.environment)
            .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
        add_library_plans(&mut builder, libraries)?;

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
        library_authority: VerifiedProfileLibraryAuthority,
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
            integrity: expected_integrity(&base.authenticated_client_integrity()?)?,
        })?;

        if resolved_version.libraries.len() > MAX_KNOWN_GOOD_ENTRIES {
            return Err(KnownGoodInventoryError::InputTooLarge);
        }
        if resolved_version.libraries
            != merge_libraries_prefer_first(
                library_authority.libraries(),
                &base.effective_version.libraries,
            )
        {
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
            let integrity = if let Some(proof) = library_authority.entries.get(&plan.relative_path)
            {
                if proof.is_native != plan.is_native
                    || plan.expected.size != proof.size
                    || plan.expected.sha1.as_deref().is_none_or(|sha1| {
                        !Sha1Digest::from_metadata(sha1)
                            .is_ok_and(|digest| digest == sha1_array_digest(&proof.sha1))
                    })
                {
                    return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
                }
                used_proofs.insert(plan.relative_path.clone());
                KnownGoodIntegrity::Sha1 {
                    digest: sha1_array_digest(&proof.sha1),
                    size: proof.size,
                }
            } else {
                let expected = expected_integrity(&plan.expected)?;
                base.inventory
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.root == KnownGoodRoot::Libraries
                            && entry.path == path
                            && entry.kind == kind
                            && entry.integrity == expected
                    })
                    .ok_or(KnownGoodInventoryError::ProfileLibraryProofMismatch)?
                    .integrity
                    .clone()
            };
            builder.insert(KnownGoodEntry {
                root: KnownGoodRoot::Libraries,
                path,
                kind,
                integrity,
            })?;
        }
        if used_proofs.len() != library_authority.entries.len() {
            return Err(KnownGoodInventoryError::ProfileLibraryProofMismatch);
        }

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

    #[expect(
        clippy::too_many_arguments,
        reason = "receipt derivation keeps each authenticated input explicit"
    )]
    pub(crate) fn from_verified_installer_source(
        base: Self,
        record: &LoaderBuildRecord,
        source: &VerifiedInstallerReceiptSource,
        resolved_version: VersionJson,
        version_bytes: &[u8],
        base_client_bytes: &[u8],
        child_client: &VerifiedInstallerClientBytes,
        loader_metadata_bytes: &[u8],
        library_authority: &VerifiedInstallerLibraryAuthority,
    ) -> Result<Self, KnownGoodInventoryError> {
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
        if !strategy_matches
            || crate::loaders::api::validate_loader_build_record_identity(record).is_err()
            || base.version_id.as_str() != record.minecraft_version
            || base.effective_version.id != record.minecraft_version
            || resolved_version.id != record.version_id
            || source.source_bytes().is_empty()
        {
            return Err(KnownGoodInventoryError::LoaderIdentityMismatch);
        }
        base.authenticate_client_bytes(base_client_bytes)?;
        if !child_client.matches_derivation(source, base_client_bytes)
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
        let expected_metadata = installed_loader_metadata_bytes(record)
            .map_err(|_| KnownGoodInventoryError::MetadataSerialization)?;
        if expected_metadata != loader_metadata_bytes {
            return Err(KnownGoodInventoryError::MetadataSerialization);
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
                size: Some(child_client_bytes.len() as u64),
            },
        })?;

        let proofs = &library_authority.entries;
        let mut used_proofs = BTreeMap::new();
        let final_libraries =
            library_artifact_plans_for(&resolved_version.libraries, &base.environment)
                .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
        for plan in final_libraries {
            let path = KnownGoodRelativePath::new(plan.relative_path.as_str())?;
            let kind = if plan.is_native {
                KnownGoodArtifactKind::NativeLibrary
            } else {
                KnownGoodArtifactKind::Library
            };
            let integrity = if let Some(proof) = proofs.get(&plan.relative_path) {
                if plan
                    .expected
                    .size
                    .is_some_and(|size| proof.size != Some(size))
                    || plan.expected.sha1.as_deref().is_some_and(|sha1| {
                        !Sha1Digest::from_metadata(sha1)
                            .is_ok_and(|expected| expected == sha1_array_digest(&proof.sha1))
                    })
                {
                    return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
                }
                KnownGoodIntegrity::Sha1 {
                    digest: sha1_array_digest(&proof.sha1),
                    size: proof.size,
                }
            } else {
                let expected = expected_integrity(&plan.expected)?;
                let base_entry = base.inventory.entries.iter().find(|entry| {
                    entry.root == KnownGoodRoot::Libraries
                        && entry.path.as_str() == path.as_str()
                        && entry.kind == kind
                        && entry.integrity == expected
                });
                base_entry
                    .ok_or(KnownGoodInventoryError::InstallerLibraryProofMismatch)?
                    .integrity
                    .clone()
            };
            builder.insert(KnownGoodEntry {
                root: KnownGoodRoot::Libraries,
                path,
                kind,
                integrity,
            })?;
            if proofs.contains_key(&plan.relative_path) {
                used_proofs.insert(plan.relative_path, ());
            }
        }

        let installer_libraries =
            library_artifact_plans_for(source.libraries(), &crate::rules::default_environment())
                .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
        for plan in installer_libraries {
            if used_proofs.contains_key(&plan.relative_path) {
                continue;
            }
            let proof = proofs
                .get(&plan.relative_path)
                .ok_or(KnownGoodInventoryError::InstallerLibraryProofMismatch)?;
            if plan
                .expected
                .size
                .is_some_and(|size| proof.size != Some(size))
                || plan.expected.sha1.as_deref().is_some_and(|sha1| {
                    !Sha1Digest::from_metadata(sha1)
                        .is_ok_and(|expected| expected == sha1_array_digest(&proof.sha1))
                })
            {
                return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
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
                    digest: sha1_array_digest(&proof.sha1),
                    size: proof.size,
                },
            })?;
            used_proofs.insert(plan.relative_path, ());
        }
        if used_proofs.len() != proofs.len() {
            return Err(KnownGoodInventoryError::InstallerLibraryProofMismatch);
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
            environment: base.environment,
        })
    }

    pub(crate) fn from_verified_vanilla_source(
        input: KnownGoodInventoryInput<'_>,
        version_json_bytes: &[u8],
    ) -> Result<Self, KnownGoodInventoryError> {
        validate_bytes(
            version_json_bytes,
            &ExpectedIntegrity::from_sha1(&input.shape.version_manifest.sha1),
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
pub struct KnownGoodInstallShape<'a> {
    pub(crate) version_manifest: &'a ManifestEntry,
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
    ProfileLibraryProofMismatch,
    InstallerLibraryProofMismatch,
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
        integrity: expected_integrity(&ExpectedIntegrity::from_mojang(client.size, &client.sha1))?,
    })?;

    if input.resolved_version.libraries.len() > MAX_KNOWN_GOOD_ENTRIES {
        return Err(KnownGoodInventoryError::InputTooLarge);
    }
    let libraries =
        library_artifact_plans_for(&input.resolved_version.libraries, input.environment)
            .map_err(|_| KnownGoodInventoryError::InvalidLibraryPlan)?;
    add_library_plans(&mut builder, libraries)?;

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
            integrity: expected_integrity(&ExpectedIntegrity::from_mojang(
                logging.file.size,
                &logging.file.sha1,
            ))?,
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
    Ok(builder.finish())
}

fn version_metadata_integrity(
    shape: KnownGoodInstallShape<'_>,
    version: &VersionJson,
) -> Result<KnownGoodIntegrity, KnownGoodInventoryError> {
    if shape.version_manifest.id != version.id {
        return Err(KnownGoodInventoryError::VersionIdentityMismatch);
    }
    expected_integrity(&ExpectedIntegrity::from_sha1(&shape.version_manifest.sha1))
}

fn add_library_plans(
    builder: &mut InventoryBuilder,
    plans: Vec<LibraryArtifactPlan>,
) -> Result<(), KnownGoodInventoryError> {
    for plan in plans {
        let path = KnownGoodRelativePath::new(plan.relative_path.as_str())?;
        let integrity = expected_integrity(&plan.expected)?;
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
        integrity: expected_integrity(&expected)?,
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
    match expected.sha1.as_deref() {
        Some(value) => Ok(KnownGoodIntegrity::Sha1 {
            digest: Sha1Digest::from_metadata(value)?,
            size: expected.size,
        }),
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
        build_id_for, installed_version_id_for,
    };
    use crate::rules::Rule;
    use std::collections::HashMap;

    struct InstallerReceiptFixture {
        base: KnownGoodInstallReceipt,
        record: LoaderBuildRecord,
        source: VerifiedInstallerReceiptSource,
        child_client: VerifiedInstallerClientBytes,
        authority: VerifiedInstallerLibraryAuthority,
        resolved: VersionJson,
        version_bytes: Vec<u8>,
        base_client_bytes: Vec<u8>,
        metadata: Vec<u8>,
        embedded_path: ArtifactRelativePath,
        terminal_path: ArtifactRelativePath,
        processor_path: ArtifactRelativePath,
        embedded_bytes: Vec<u8>,
        terminal_bytes: Vec<u8>,
        processor_bytes: Vec<u8>,
    }

    fn installer_receipt_fixture(base_client_bytes: &[u8]) -> InstallerReceiptFixture {
        let record = loader_record(FixtureShape::Forge, LoaderComponentId::Forge);
        let embedded_bytes = b"authenticated embedded library".to_vec();
        let terminal_bytes = b"authenticated terminal processor output".to_vec();
        let processor_bytes = b"authenticated processor-only library".to_vec();
        let embedded_path = ArtifactRelativePath::new("example/embedded/1.0/embedded-1.0.jar")
            .expect("embedded path");
        let terminal_path = ArtifactRelativePath::new("example/terminal/1.0/terminal-1.0.jar")
            .expect("terminal path");
        let processor_path =
            ArtifactRelativePath::new("example/processor-only/1.0/processor-only-1.0.jar")
                .expect("processor-only path");
        let libraries = vec![
            checksum_library(
                "example:embedded:1.0",
                embedded_path.as_str(),
                sha1_digest(&embedded_bytes).as_str(),
                embedded_bytes.len() as i64,
            ),
            checksum_library(
                "example:terminal:1.0",
                terminal_path.as_str(),
                sha1_digest(&terminal_bytes).as_str(),
                terminal_bytes.len() as i64,
            ),
        ];
        let mut installer_libraries = libraries.clone();
        installer_libraries.push(checksum_library(
            "example:processor-only:1.0",
            processor_path.as_str(),
            sha1_digest(&processor_bytes).as_str(),
            processor_bytes.len() as i64,
        ));
        let source = VerifiedInstallerReceiptSource::from_test(
            libraries,
            installer_libraries,
            vec![
                (embedded_path.clone(), embedded_bytes.clone()),
                (processor_path.clone(), processor_bytes.clone()),
            ],
            false,
        );
        let child_client = source
            .derive_child_client_bytes(base_client_bytes)
            .expect("derived child client");
        let authority = seal_verified_installer_library_authority(
            &source,
            Vec::new(),
            VerifiedProcessorOutputs::from_test_terminal(vec![(
                terminal_path.clone(),
                terminal_bytes.clone(),
            )]),
        )
        .expect("sealed library authority");

        let mut base_version = fixture(FixtureShape::Vanilla, false).version;
        base_version.id = record.minecraft_version.clone();
        base_version.libraries.clear();
        let client = base_version
            .downloads
            .client
            .as_mut()
            .expect("base client metadata");
        client.sha1 = sha1_digest(base_client_bytes).as_str().to_string();
        client.size = base_client_bytes.len() as i64;
        let base = KnownGoodInstallReceipt {
            version_id: KnownGoodId::new(&record.minecraft_version).expect("base id"),
            inventory: InventoryBuilder::default().finish(),
            effective_version: base_version.clone(),
            environment: crate::rules::default_environment(),
        };
        let mut resolved = compose_loader_version(
            &base_version,
            &record.minecraft_version,
            &record.version_id,
            source.version(),
        )
        .expect("resolved installer version");
        let resolved_client = resolved.downloads.client.as_mut().expect("resolved client");
        resolved_client.sha1 = sha1_digest(child_client.bytes()).as_str().to_string();
        resolved_client.size = child_client.bytes().len() as i64;
        resolved_client.url.clear();
        let version_bytes = serde_json::to_vec_pretty(&resolved).expect("version bytes");
        let metadata = installed_loader_metadata_bytes(&record).expect("loader metadata");

        InstallerReceiptFixture {
            base,
            record,
            source,
            child_client,
            authority,
            resolved,
            version_bytes,
            base_client_bytes: base_client_bytes.to_vec(),
            metadata,
            embedded_path,
            terminal_path,
            processor_path,
            embedded_bytes,
            terminal_bytes,
            processor_bytes,
        }
    }

    #[test]
    fn installer_authority_rejects_missing_duplicate_and_extra_proofs() {
        let path =
            ArtifactRelativePath::new("example/proof/1.0/proof-1.0.jar").expect("proof path");
        let bytes = b"proof bytes".to_vec();
        let library = checksum_library(
            "example:proof:1.0",
            path.as_str(),
            sha1_digest(&bytes).as_str(),
            bytes.len() as i64,
        );
        let source = VerifiedInstallerReceiptSource::from_test(
            Vec::new(),
            vec![library],
            vec![(path.clone(), bytes.clone())],
            false,
        );

        assert!(matches!(
            seal_verified_installer_library_authority(
                &source,
                Vec::new(),
                VerifiedProcessorOutputs::from_test_terminal(vec![(path, bytes)]),
            ),
            Err(KnownGoodInventoryError::InstallerLibraryProofMismatch)
        ));

        let missing = VerifiedInstallerReceiptSource::from_test(
            Vec::new(),
            source.libraries().to_vec(),
            Vec::new(),
            false,
        );
        assert!(matches!(
            seal_verified_installer_library_authority(
                &missing,
                Vec::new(),
                VerifiedProcessorOutputs::none(),
            ),
            Err(KnownGoodInventoryError::InstallerLibraryProofMismatch)
        ));

        let extra_path =
            ArtifactRelativePath::new("example/extra/1.0/extra-1.0.jar").expect("extra path");
        let empty =
            VerifiedInstallerReceiptSource::from_test(Vec::new(), Vec::new(), Vec::new(), false);
        assert!(matches!(
            seal_verified_installer_library_authority(
                &empty,
                Vec::new(),
                VerifiedProcessorOutputs::from_test_terminal(vec![
                    (extra_path, b"extra".to_vec(),)
                ]),
            ),
            Err(KnownGoodInventoryError::InstallerLibraryProofMismatch)
        ));

        let declared = b"declared";
        let tampered = b"tampered";
        let drift_path =
            ArtifactRelativePath::new("example/drift/1.0/drift-1.0.jar").expect("drift path");
        let digest_drift = VerifiedInstallerReceiptSource::from_test(
            Vec::new(),
            vec![checksum_library(
                "example:drift:1.0",
                drift_path.as_str(),
                sha1_digest(declared).as_str(),
                declared.len() as i64,
            )],
            Vec::new(),
            false,
        );
        assert!(matches!(
            seal_verified_installer_library_authority(
                &digest_drift,
                Vec::new(),
                VerifiedProcessorOutputs::from_test_terminal(vec![(
                    drift_path.clone(),
                    tampered.to_vec(),
                )]),
            ),
            Err(KnownGoodInventoryError::InstallerLibraryProofMismatch)
        ));

        let size_drift = VerifiedInstallerReceiptSource::from_test(
            Vec::new(),
            vec![checksum_library(
                "example:drift:1.0",
                drift_path.as_str(),
                sha1_digest(tampered).as_str(),
                tampered.len() as i64 + 1,
            )],
            Vec::new(),
            false,
        );
        assert!(matches!(
            seal_verified_installer_library_authority(
                &size_drift,
                Vec::new(),
                VerifiedProcessorOutputs::from_test_terminal(vec![
                    (drift_path, tampered.to_vec(),)
                ]),
            ),
            Err(KnownGoodInventoryError::InstallerLibraryProofMismatch)
        ));
    }

    #[test]
    fn installer_authority_seals_processor_only_embedded_artifacts() {
        let processor_path = ArtifactRelativePath::new("example/processor/1.0/processor-1.0.jar")
            .expect("processor path");
        let processor_bytes = b"processor input".to_vec();
        let source = VerifiedInstallerReceiptSource::from_test(
            Vec::new(),
            vec![checksum_library(
                "example:processor:1.0",
                processor_path.as_str(),
                sha1_digest(&processor_bytes).as_str(),
                processor_bytes.len() as i64,
            )],
            vec![(processor_path, processor_bytes)],
            false,
        );
        let authority = seal_verified_installer_library_authority(
            &source,
            Vec::new(),
            VerifiedProcessorOutputs::none(),
        )
        .expect("processor-only embedded input authority");
        assert!(authority.into_terminal_materializations().is_empty());
        assert_eq!(source.into_embedded_maven_artifacts().len(), 1);
    }

    #[test]
    fn installer_receipt_is_exact_and_retains_materialization_carriers() {
        let fixture = installer_receipt_fixture(b"authenticated base client");
        let InstallerReceiptFixture {
            base,
            record,
            source,
            child_client,
            authority,
            resolved,
            version_bytes,
            base_client_bytes,
            metadata,
            embedded_path,
            terminal_path,
            processor_path,
            embedded_bytes,
            terminal_bytes,
            processor_bytes,
        } = fixture;
        assert!(
            !resolved
                .libraries
                .iter()
                .any(|library| library.name == "example:processor-only:1.0")
        );
        let receipt = KnownGoodInstallReceipt::from_verified_installer_source(
            base,
            &record,
            &source,
            resolved,
            &version_bytes,
            &base_client_bytes,
            &child_client,
            &metadata,
            &authority,
        )
        .expect("verified installer receipt");

        assert_eq!(child_client.into_bytes(), base_client_bytes);
        let embedded = source.into_embedded_maven_artifacts();
        assert_eq!(embedded.len(), 2);
        assert!(embedded.iter().any(|artifact| {
            artifact.relative_path() == &embedded_path && artifact.bytes() == embedded_bytes
        }));
        assert!(embedded.iter().any(|artifact| {
            artifact.relative_path() == &processor_path && artifact.bytes() == processor_bytes
        }));
        let terminal = authority.into_terminal_materializations();
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].0, terminal_path);
        assert_eq!(terminal[0].1.as_ref(), terminal_bytes);

        let inventory = receipt.into_inventory();
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            embedded_path.as_str(),
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: sha1_digest(&embedded_bytes),
                size: Some(embedded_bytes.len() as u64),
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            processor_path.as_str(),
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: sha1_digest(&processor_bytes),
                size: Some(processor_bytes.len() as u64),
            },
        );
        assert_entry(
            &inventory,
            &KnownGoodRoot::Libraries,
            terminal_path.as_str(),
            KnownGoodArtifactKind::Library,
            &KnownGoodIntegrity::Sha1 {
                digest: sha1_digest(&terminal_bytes),
                size: Some(terminal_bytes.len() as u64),
            },
        );
    }

    #[test]
    fn installer_receipt_rejects_identity_library_and_cross_base_drift() {
        let mut identity = installer_receipt_fixture(b"authenticated base client");
        identity.resolved.main_class = "tampered.Main".to_string();
        identity.version_bytes = serde_json::to_vec_pretty(&identity.resolved).expect("version");
        assert!(matches!(
            KnownGoodInstallReceipt::from_verified_installer_source(
                identity.base,
                &identity.record,
                &identity.source,
                identity.resolved,
                &identity.version_bytes,
                &identity.base_client_bytes,
                &identity.child_client,
                &identity.metadata,
                &identity.authority,
            ),
            Err(KnownGoodInventoryError::LoaderIdentityMismatch)
        ));

        let cross_base = installer_receipt_fixture(b"authenticated base client");
        let foreign_source =
            VerifiedInstallerReceiptSource::from_test(Vec::new(), Vec::new(), Vec::new(), false);
        let foreign_client = foreign_source
            .derive_child_client_bytes(b"different authenticated base")
            .expect("foreign child client");
        assert!(matches!(
            KnownGoodInstallReceipt::from_verified_installer_source(
                cross_base.base,
                &cross_base.record,
                &cross_base.source,
                cross_base.resolved,
                &cross_base.version_bytes,
                &cross_base.base_client_bytes,
                &foreign_client,
                &cross_base.metadata,
                &cross_base.authority,
            ),
            Err(KnownGoodInventoryError::ClientIntegrity)
        ));
    }

    #[test]
    fn profile_library_sealer_is_exact_and_authors_both_metadata_locations() {
        let path = "org/example/profile/1/profile-1.jar";
        let library = Library {
            name: "org.example:profile:1".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: path.to_string(),
                    url: "https://example.invalid/profile.jar".to_string(),
                    ..LibraryArtifact::default()
                }),
                classifiers: HashMap::new(),
            }),
            ..Library::default()
        };
        let proof = || {
            ExactLibraryDownloadProof::new_for_test(
                ArtifactRelativePath::new(path).expect("profile path"),
                Some(23),
                [0xbb; 20],
            )
        };

        assert_eq!(
            seal_verified_profile_library_authority(std::slice::from_ref(&library), Vec::new())
                .err()
                .expect("missing proof"),
            KnownGoodInventoryError::ProfileLibraryProofMismatch
        );
        assert_eq!(
            seal_verified_profile_library_authority(
                std::slice::from_ref(&library),
                vec![proof(), proof()],
            )
            .err()
            .expect("duplicate proof"),
            KnownGoodInventoryError::ProfileLibraryProofMismatch
        );
        assert_eq!(
            seal_verified_profile_library_authority(
                std::slice::from_ref(&library),
                vec![ExactLibraryDownloadProof::new_for_test(
                    ArtifactRelativePath::new("org/example/extra/1/extra-1.jar")
                        .expect("extra path"),
                    Some(23),
                    [0xbb; 20],
                )],
            )
            .err()
            .expect("extra proof"),
            KnownGoodInventoryError::ProfileLibraryProofMismatch
        );

        let authority = seal_verified_profile_library_authority(&[library], vec![proof()])
            .expect("sealed profile proof");
        let enriched = &authority.libraries()[0];
        assert_eq!(enriched.sha1, SHA_B);
        assert_eq!(enriched.size, 23);
        let artifact = enriched
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .expect("download artifact");
        assert_eq!(artifact.sha1, SHA_B);
        assert_eq!(artifact.size, 23);

        let mut declared = enriched.clone();
        assert_eq!(
            seal_verified_profile_library_authority(
                std::slice::from_ref(&declared),
                vec![ExactLibraryDownloadProof::new_for_test(
                    ArtifactRelativePath::new(path).expect("profile path"),
                    Some(24),
                    [0xbb; 20],
                )],
            )
            .err()
            .expect("declared size drift"),
            KnownGoodInventoryError::ProfileLibraryProofMismatch
        );
        assert_eq!(
            seal_verified_profile_library_authority(
                std::slice::from_ref(&declared),
                vec![ExactLibraryDownloadProof::new_for_test(
                    ArtifactRelativePath::new(path).expect("profile path"),
                    Some(23),
                    [0xaa; 20],
                )],
            )
            .err()
            .expect("declared digest drift"),
            KnownGoodInventoryError::ProfileLibraryProofMismatch
        );
        declared
            .downloads
            .as_mut()
            .and_then(|downloads| downloads.artifact.as_mut())
            .expect("download artifact")
            .sha1 = SHA_A.to_string();
        assert_eq!(
            seal_verified_profile_library_authority(&[declared], vec![proof()])
                .err()
                .expect("conflicting nested metadata"),
            KnownGoodInventoryError::InvalidLibraryPlan
        );

        let mut disallowed = enriched.clone();
        disallowed.rules = vec![Rule {
            action: "disallow".to_string(),
            os: None,
            features: None,
        }];
        assert_eq!(
            seal_verified_profile_library_authority(&[disallowed], vec![proof()])
                .err()
                .expect("current environment drift"),
            KnownGoodInventoryError::ProfileLibraryProofMismatch
        );
    }

    #[test]
    fn profile_library_sealer_authors_primary_and_native_contracts_independently() {
        let environment = crate::rules::default_environment();
        let classifier = crate::rules::native_classifier_key();
        let primary_path = "org/example/dual/1/dual-1.jar";
        let native_path = format!("org/example/dual/1/dual-1-{classifier}.jar");
        let library = Library {
            name: "org.example:dual:1".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: primary_path.to_string(),
                    url: "https://example.invalid/dual.jar".to_string(),
                    ..LibraryArtifact::default()
                }),
                classifiers: HashMap::from([(
                    classifier.clone(),
                    LibraryArtifact {
                        path: native_path.clone(),
                        url: "https://example.invalid/dual-native.jar".to_string(),
                        ..LibraryArtifact::default()
                    },
                )]),
            }),
            natives: HashMap::from([(environment.os_name, classifier.clone())]),
            ..Library::default()
        };
        let authority = seal_verified_profile_library_authority(
            &[library],
            vec![
                ExactLibraryDownloadProof::new_for_test(
                    ArtifactRelativePath::new(primary_path).expect("primary path"),
                    Some(31),
                    [0xaa; 20],
                ),
                ExactLibraryDownloadProof::new_for_test(
                    ArtifactRelativePath::new(&native_path).expect("native path"),
                    Some(47),
                    [0xcc; 20],
                ),
            ],
        )
        .expect("primary and native authority");
        let enriched = &authority.libraries()[0];
        assert_eq!(enriched.sha1, SHA_A);
        assert_eq!(enriched.size, 31);
        let downloads = enriched.downloads.as_ref().expect("downloads");
        let primary = downloads.artifact.as_ref().expect("primary artifact");
        assert_eq!(primary.sha1, SHA_A);
        assert_eq!(primary.size, 31);
        let native = downloads
            .classifiers
            .get(&classifier)
            .expect("native artifact");
        assert_eq!(native.sha1, SHA_C);
        assert_eq!(native.size, 47);
    }

    #[test]
    fn fabric_and_quilt_profile_receipts_bind_authored_checksumless_proofs() {
        for shape in [FixtureShape::Fabric, FixtureShape::Quilt] {
            let fixture = fixture(shape, false);
            let record = fixture.loader_record.as_ref().expect("loader record");
            let coordinate = format!("org.example:{}-proof:1", record.component_id.as_str());
            let path = format!(
                "org/example/{0}-proof/1/{0}-proof-1.jar",
                record.component_id.as_str()
            );
            let library = Library {
                name: coordinate,
                downloads: Some(LibraryDownload {
                    artifact: Some(LibraryArtifact {
                        path: path.clone(),
                        url: "https://example.invalid/profile.jar".to_string(),
                        ..LibraryArtifact::default()
                    }),
                    classifiers: HashMap::new(),
                }),
                ..Library::default()
            };
            let authority = seal_verified_profile_library_authority(
                &[library],
                vec![ExactLibraryDownloadProof::new_for_test(
                    ArtifactRelativePath::new(&path).expect("profile path"),
                    Some(53),
                    [0xcc; 20],
                )],
            )
            .expect("profile authority");
            let authored = authority.libraries()[0].clone();
            assert_eq!(authored.sha1, SHA_C);
            assert_eq!(authored.size, 53);
            let artifact = authored
                .downloads
                .as_ref()
                .and_then(|downloads| downloads.artifact.as_ref())
                .expect("authored artifact");
            assert_eq!(artifact.sha1, SHA_C);
            assert_eq!(artifact.size, 53);

            let mut base_version = fixture.version.clone();
            base_version.id = record.minecraft_version.clone();
            base_version.inherits_from.clear();
            base_version.materialized = false;
            base_version.libraries.clear();
            let base = KnownGoodInstallReceipt {
                version_id: KnownGoodId::new(&record.minecraft_version).expect("base id"),
                inventory: InventoryBuilder::default().finish(),
                effective_version: base_version.clone(),
                environment: fixture.environment.clone(),
            };
            let mut child = base_version;
            child.id = record.version_id.clone();
            child.inherits_from = record.minecraft_version.clone();
            child.materialized = true;
            child.libraries = vec![authored];
            let version_bytes = serde_json::to_vec_pretty(&child).expect("written version");
            let written: VersionJson =
                serde_json::from_slice(&version_bytes).expect("parse written version");
            let written_artifact = written.libraries[0]
                .downloads
                .as_ref()
                .and_then(|downloads| downloads.artifact.as_ref())
                .expect("written artifact");
            assert_eq!(written.libraries[0].sha1, SHA_C);
            assert_eq!(written.libraries[0].size, 53);
            assert_eq!(written_artifact.sha1, SHA_C);
            assert_eq!(written_artifact.size, 53);
            let metadata = installed_loader_metadata_bytes(record).expect("loader metadata");
            let receipt = KnownGoodInstallReceipt::from_verified_profile_source(
                &base,
                record,
                child,
                &version_bytes,
                authority,
                &metadata,
            )
            .expect("profile receipt");
            assert!(receipt.inventory.entries().iter().any(|entry| {
                entry.path().as_str() == path
                    && entry.integrity()
                        == &KnownGoodIntegrity::Sha1 {
                            digest: Sha1Digest::from_metadata(SHA_C).expect("digest"),
                            size: Some(53),
                        }
            }));
        }
    }

    #[test]
    fn profile_receipt_rejects_checksumless_inherited_base_library() {
        let fixture = fixture(FixtureShape::Fabric, false);
        let record = fixture.loader_record.as_ref().expect("loader record");
        let profile = Library {
            name: "org.example:profile-proof:1".to_string(),
            url: "https://example.invalid/maven/".to_string(),
            ..Library::default()
        };
        let authority = seal_verified_profile_library_authority(
            std::slice::from_ref(&profile),
            vec![ExactLibraryDownloadProof::new_for_test(
                ArtifactRelativePath::new("org/example/profile-proof/1/profile-proof-1.jar")
                    .expect("profile path"),
                Some(17),
                [0xaa; 20],
            )],
        )
        .expect("profile authority");
        let mut base_version = fixture.version.clone();
        base_version.id = record.minecraft_version.clone();
        base_version.inherits_from.clear();
        base_version.materialized = false;
        base_version.libraries = vec![Library {
            name: "org.example:unproved-base:1".to_string(),
            url: "https://example.invalid/maven/".to_string(),
            ..Library::default()
        }];
        let base = KnownGoodInstallReceipt {
            version_id: KnownGoodId::new(&record.minecraft_version).expect("base id"),
            inventory: InventoryBuilder::default().finish(),
            effective_version: base_version.clone(),
            environment: fixture.environment,
        };
        let mut child = base_version;
        child.id = record.version_id.clone();
        child.inherits_from = record.minecraft_version.clone();
        child.materialized = true;
        child.libraries =
            merge_libraries_prefer_first(authority.libraries(), &base.effective_version.libraries);
        let version_bytes = serde_json::to_vec_pretty(&child).expect("version bytes");
        let metadata = installed_loader_metadata_bytes(record).expect("loader metadata");

        assert_eq!(
            KnownGoodInstallReceipt::from_verified_profile_source(
                &base,
                record,
                child,
                &version_bytes,
                authority,
                &metadata,
            ),
            Err(KnownGoodInventoryError::MissingChecksum)
        );
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
        let authority = seal_verified_profile_library_authority(
            std::slice::from_ref(&profile_library),
            vec![ExactLibraryDownloadProof::new_for_test(
                ArtifactRelativePath::new("org/ow2/asm/asm/9.9/asm-9.9.jar").expect("profile path"),
                Some(12),
                [0xaa; 20],
            )],
        )
        .expect("profile authority");
        let mut child = fixture.version.clone();
        child.inherits_from = record.minecraft_version.clone();
        child.materialized = true;
        child.libraries = authority.libraries().to_vec();
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
            authority,
            &metadata,
        )
        .expect("profile receipt");
        let inventory = receipt.into_inventory();

        assert!(inventory.entries().iter().any(|entry| {
            entry.path().as_str() == "org/ow2/asm/asm/9.9/asm-9.9.jar"
                && matches!(
                    entry.integrity(),
                    KnownGoodIntegrity::Sha1 { size: Some(12), .. }
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
        assert_sorted_unique(&inventory);
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
    fn vanilla_version_identity_mismatch_fails_closed() {
        let mut fixture = fixture(FixtureShape::Vanilla, false);
        fixture.version_manifest.id = "different-version".to_string();

        let error = derive_known_good_inventory(fixture.input()).expect_err("version mismatch");
        assert_eq!(error, KnownGoodInventoryError::VersionIdentityMismatch);
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FixtureShape {
        Vanilla,
        Fabric,
        Quilt,
        Forge,
    }

    impl FixtureShape {
        fn component(self) -> Option<LoaderComponentId> {
            match self {
                Self::Vanilla => None,
                Self::Fabric => Some(LoaderComponentId::Fabric),
                Self::Quilt => Some(LoaderComponentId::Quilt),
                Self::Forge => Some(LoaderComponentId::Forge),
            }
        }

        fn strategy(self) -> Option<LoaderInstallStrategy> {
            match self {
                Self::Vanilla => None,
                Self::Fabric => Some(LoaderInstallStrategy::FabricProfile),
                Self::Quilt => Some(LoaderInstallStrategy::QuiltProfile),
                Self::Forge => Some(LoaderInstallStrategy::ForgeModern),
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
        loader_record: Option<LoaderBuildRecord>,
        environment: Environment,
    }

    impl Fixture {
        fn input(&self) -> KnownGoodInventoryInput<'_> {
            KnownGoodInventoryInput {
                resolved_version: &self.version,
                asset_index_bytes: Some(&self.asset_index),
                runtime: Some(RuntimeInventoryInput {
                    component: &self.runtime_id,
                    manifest_bytes: &self.runtime_manifest_bytes,
                    manifest_expected: &self.runtime_manifest_expected,
                }),
                shape: KnownGoodInstallShape {
                    version_manifest: &self.version_manifest,
                },
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
        libraries.extend(profile_libraries.iter().cloned());
        if matches!(shape, FixtureShape::Forge) {
            libraries.push(checksumless_loader_library());
        }
        if shuffled {
            libraries.reverse();
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
            loader_record,
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
            FixtureShape::Forge => (
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
