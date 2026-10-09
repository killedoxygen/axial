//! Publication settlement shared by Vanilla and loader installations.
//!
//! Materialization receipts are deliberately not readiness. The queue activates
//! verified evidence in its metadata transaction, then acknowledges publication.
//! Every noncommitted classification keeps the original authority available.

use crate::library::GenerationPin;
use axial_minecraft::known_good::{KnownGoodActivationSource, KnownGoodIntegrity, KnownGoodRoot};
use axial_minecraft::loaders::{
    LoaderInstallBaseCommit, LoaderInstallBaseCommitVerificationFailure,
    VerifiedLoaderInstallBaseCommit,
};
use axial_minecraft::managed_path::{FileAbsence, FileObservation, ManagedLibraryOperation};
use axial_minecraft::portable_path::{PortableFileName, PortableRelativePath};
use axial_minecraft::{
    KnownGoodInstallReceipt, ManagedInstallCommittedEvidence, ManagedInstallDurableOutcome,
    ManagedInstallDurableRecovery, ManagedInstallReceiptVerificationFailure,
    ManagedInstallRollbackEffect, ManagedInstallRolledBackEvidence, VerifiedManagedInstallReceipt,
};
use axial_minecraft::{VersionJson, rules::Environment};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_INVENTORY_ENTRIES: usize = 1_000_000;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Inspection {
    Summary,
    Integrity,
}

impl Inspection {
    fn observes(self, path: &str) -> bool {
        self == Self::Integrity
            || !path.starts_with("assets/")
            || is_asset_index(path)
            || path
                .strip_prefix("assets/log_configs/")
                .is_some_and(|id| PortableFileName::new_exact(id).is_ok())
    }
}

fn is_asset_index(path: &str) -> bool {
    path.strip_prefix("assets/indexes/")
        .and_then(|path| path.strip_suffix(".json"))
        .is_some_and(|id| PortableFileName::new_exact(id).is_ok())
}

/// One grouped projection's cumulative dependency checks and declared read bytes.
pub(crate) struct InventoryBudget {
    remaining_checks: u64,
    remaining_bytes: u64,
}

impl InventoryBudget {
    pub(crate) fn projection() -> Self {
        Self {
            remaining_checks: MAX_INVENTORY_ENTRIES as u64,
            remaining_bytes: axial_minecraft::known_good::MAX_TIER2_AGGREGATE_BYTES,
        }
    }

    pub(crate) fn reserve_record(&mut self, bytes: u64) -> Result<(), super::queue::InstallError> {
        self.reserve(0, bytes)
    }

    pub(crate) fn reserve_checks(
        &mut self,
        entries: u64,
    ) -> Result<(), super::queue::InstallError> {
        self.reserve(entries, 0)
    }

    pub(crate) fn reserve_inventory(
        &mut self,
        inventory: &ActivatedVersion,
        inspection: Inspection,
    ) -> Result<(), super::queue::InstallError> {
        use super::queue::InstallError;
        let checks = (inventory.files.len() as u64)
            .checked_add(
                inventory
                    .files
                    .iter()
                    .filter(|file| inspection.observes(&file.path))
                    .count() as u64,
            )
            .ok_or(InstallError::AtCapacity)?;
        if checks > self.remaining_checks {
            return Err(InstallError::AtCapacity);
        }
        let metadata = format!("versions/{0}/{0}.json", inventory.version_id);
        let mut bytes = 0_u64;
        for file in &inventory.files {
            if inspection == Inspection::Integrity || file.path == metadata {
                bytes = bytes
                    .checked_add(file.size)
                    .and_then(|bytes| bytes.checked_add(2))
                    .ok_or(InstallError::AtCapacity)?;
            }
            if file.path == metadata
                || (inspection == Inspection::Integrity && is_asset_index(&file.path))
            {
                // Bounded JSON reads also perform EOF and completion probes.
                bytes = bytes
                    .checked_add(file.size)
                    .and_then(|bytes| bytes.checked_add(2))
                    .ok_or(InstallError::AtCapacity)?;
            }
        }
        self.reserve(checks, bytes)
    }

    fn reserve(&mut self, checks: u64, bytes: u64) -> Result<(), super::queue::InstallError> {
        use super::queue::InstallError;
        let remaining_checks = self
            .remaining_checks
            .checked_sub(checks)
            .ok_or(InstallError::AtCapacity)?;
        let remaining_bytes = self
            .remaining_bytes
            .checked_sub(bytes)
            .ok_or(InstallError::AtCapacity)?;
        self.remaining_checks = remaining_checks;
        self.remaining_bytes = remaining_bytes;
        Ok(())
    }
}

/// Private durable projection, produced exclusively by verified activation. A
/// serialized record is never itself filesystem authority. Summary observations
/// are distinct from the complete verified receipts required for launch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivatedVersion {
    pub version_id: String,
    pub contract_id: String,
    #[serde(deserialize_with = "deserialize_files")]
    pub files: Vec<ActivatedFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivatedFile {
    pub path: String,
    pub sha1: String,
    pub size: u64,
}

fn deserialize_files<'de, D>(deserializer: D) -> Result<Vec<ActivatedFile>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Files;

    impl<'de> serde::de::Visitor<'de> for Files {
        type Value = Vec<ActivatedFile>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded installed-file inventory")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            use serde::de::Error;

            let mut files = Vec::new();
            while files.len() < MAX_INVENTORY_ENTRIES {
                if files.len() == files.capacity() {
                    let additional = files
                        .capacity()
                        .max(1)
                        .min(MAX_INVENTORY_ENTRIES - files.len());
                    files
                        .try_reserve_exact(additional)
                        .map_err(|_| A::Error::custom("installed inventory allocation failed"))?;
                }
                let Some(file) = sequence.next_element()? else {
                    return Ok(files);
                };
                files.push(file);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(A::Error::custom("installed inventory exceeds entry limit"));
            }
            Ok(files)
        }
    }

    deserializer.deserialize_seq(Files)
}

impl ActivatedVersion {
    pub(crate) fn from_source(source: &KnownGoodActivationSource) -> Self {
        let files = source
            .inventory()
            .entries()
            .iter()
            .filter_map(|entry| {
                let root = match entry.root() {
                    KnownGoodRoot::Versions => "versions",
                    KnownGoodRoot::Libraries => "libraries",
                    KnownGoodRoot::Assets => "assets",
                    // Java's own receipt verifies its tree and executable at launch.
                    KnownGoodRoot::ManagedRuntime { .. } => return None,
                };
                let (digest, size) = match entry.integrity() {
                    KnownGoodIntegrity::Sha1 { digest, size }
                    | KnownGoodIntegrity::ExactBytes { digest, size } => (digest, size),
                    _ => return None,
                };
                Some(ActivatedFile {
                    path: format!("{root}/{}", entry.path().as_str()),
                    sha1: digest.as_str().to_owned(),
                    size: *size,
                })
            })
            .collect();
        Self {
            version_id: source.version_id().into(),
            contract_id: source.activation_contract_id().as_str().into(),
            files,
        }
    }

    pub(crate) fn verify(
        self,
        pin: GenerationPin,
    ) -> Result<InstalledVersionReceipt, super::queue::InstallError> {
        self.inspect(pin, Inspection::Integrity, false)?
            .into_ready()
    }

    pub(crate) fn inspect(
        mut self,
        pin: GenerationPin,
        inspection: Inspection,
        diagnostics: bool,
    ) -> Result<VersionInspection, super::queue::InstallError> {
        use super::queue::InstallError;
        axial_minecraft::ManagedInstallActivationContractId::parse(&self.contract_id)
            .map_err(|_| InstallError::NotReady)?;
        if self.files.is_empty() || self.files.len() > MAX_INVENTORY_ENTRIES {
            return Err(InstallError::NotReady);
        }
        let operation = pin
            .managed_library()
            .map_err(|_| InstallError::LibraryUnavailable)?;
        self.files
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let mut batch = operation.file_batch();
        let metadata_path = format!("versions/{0}/{0}.json", self.version_id);
        let client = format!("versions/{0}/{0}.jar", self.version_id);
        let client_path =
            PortableRelativePath::new_exact(&client).map_err(|_| InstallError::NotReady)?;
        let mut guards = Vec::with_capacity(
            self.files
                .iter()
                .filter(|file| inspection.observes(&file.path))
                .count(),
        );
        let mut missing = Vec::new();
        let mut reasons = Vec::new();
        let mut version = None;
        let mut exact = BTreeMap::new();
        let mut asset_flags = BTreeMap::new();
        for expected in &self.files {
            if expected.size > 2 * 1024 * 1024 * 1024 {
                return Err(InstallError::NotReady);
            }
            let mut expected_digest = [0; 20];
            hex::decode_to_slice(&expected.sha1, &mut expected_digest)
                .map_err(|_| InstallError::NotReady)?;
            if hex::encode(expected_digest) != expected.sha1 {
                return Err(InstallError::NotReady);
            }
            let path = PortableRelativePath::new_exact(&expected.path)
                .map_err(|_| InstallError::NotReady)?;
            let root = expected.path.split('/').next();
            if !matches!(root, Some("versions" | "libraries" | "assets"))
                || exact
                    .insert(
                        expected.path.clone(),
                        (expected.sha1.clone(), expected.size),
                    )
                    .is_some()
            {
                return Err(InstallError::NotReady);
            }
            let is_asset_index = is_asset_index(&expected.path);
            let is_required_library = root == Some("libraries")
                || expected
                    .path
                    .strip_prefix("assets/log_configs/")
                    .is_some_and(|id| PortableFileName::new_exact(id).is_ok());
            if !inspection.observes(&expected.path) {
                continue;
            }
            let file = match batch
                .observe_file_with_absence(&path)
                .map_err(|_| InstallError::NotReady)?
            {
                FileObservation::Present(file) => file,
                FileObservation::Missing(absence) => {
                    let reason = if path == client_path {
                        InstallError::ClientJarMissing
                    } else if expected.path == metadata_path {
                        InstallError::VersionJsonMissing
                    } else if is_required_library {
                        InstallError::LibrariesMissing
                    } else if is_asset_index {
                        InstallError::AssetIndexMissing
                    } else {
                        InstallError::NotReady
                    };
                    if !diagnostics || reason == InstallError::NotReady {
                        return Err(reason);
                    }
                    if !reasons.contains(&reason) {
                        reasons.push(reason);
                        missing.push(absence);
                    }
                    continue;
                }
            };
            if file.size() != expected.size
                || ((inspection == Inspection::Integrity || expected.path == metadata_path)
                    && file
                        .sha1_bounded(expected.size)
                        .map_err(|_| InstallError::NotReady)?
                        != expected_digest)
            {
                let reason = if path == client_path {
                    InstallError::ClientJarCorrupt
                } else if is_required_library {
                    InstallError::LibrariesCorrupt
                } else if is_asset_index {
                    InstallError::AssetIndexCorrupt
                } else {
                    InstallError::NotReady
                };
                if !diagnostics || reason == InstallError::NotReady {
                    return Err(reason);
                }
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
                guards.push((path, file.revision_observation()));
                continue;
            }
            if expected.path == metadata_path {
                let bytes = file
                    .read_bounded(16 << 20)
                    .map_err(|_| InstallError::NotReady)?;
                version = Some(
                    serde_json::from_slice::<VersionJson>(&bytes)
                        .map_err(|_| InstallError::NotReady)?,
                );
            }
            if is_asset_index && inspection == Inspection::Integrity {
                #[derive(Deserialize)]
                struct AssetFlags {
                    #[serde(default, rename = "virtual")]
                    virtual_assets: bool,
                    #[serde(default)]
                    map_to_resources: bool,
                }
                let bytes = file
                    .read_bounded(64 << 20)
                    .map_err(|_| InstallError::NotReady)?;
                let flags: AssetFlags =
                    serde_json::from_slice(&bytes).map_err(|_| InstallError::NotReady)?;
                asset_flags.insert(
                    expected.path.clone(),
                    flags.virtual_assets || flags.map_to_resources,
                );
            }
            // Retain compact revision evidence, not an open descriptor for
            // every asset object. Re-observation below is bounded to one file.
            guards.push((path, file.revision_observation()));
        }
        if !exact.contains_key(&client) || !exact.contains_key(&metadata_path) {
            return Err(InstallError::NotReady);
        }
        let virtual_assets = if let Some(version) = version.as_mut() {
            if version.id != self.version_id
                || (!version.inherits_from.is_empty() && !version.materialized)
            {
                return Err(InstallError::NotReady);
            }
            version.java_version = axial_minecraft::effective_java_version_for(
                &version.id,
                &version.kind,
                &version.java_version,
            );
            if version.asset_index.id.is_empty() && !version.assets.is_empty() {
                version.asset_index.id = version.assets.clone();
            }
            if version.asset_index.id.is_empty() {
                Some(false)
            } else {
                PortableFileName::new_exact(&version.asset_index.id)
                    .map_err(|_| InstallError::NotReady)?;
                let index = format!("assets/indexes/{}.json", version.asset_index.id);
                if !exact.contains_key(&index) {
                    return Err(InstallError::NotReady);
                }
                if inspection == Inspection::Summary {
                    Some(false)
                } else {
                    asset_flags.get(&index).copied()
                }
            }
        } else {
            None
        };
        let evidence = InventoryEvidence {
            pin,
            operation,
            guards,
            missing,
            scratch: None,
        };
        if !reasons.is_empty() {
            evidence.revalidate()?;
            return Ok(VersionInspection::Damaged(ObservedDamage {
                evidence,
                reasons,
            }));
        }
        let version = version.ok_or(InstallError::NotReady)?;
        if inspection == Inspection::Summary {
            evidence.revalidate()?;
            return Ok(VersionInspection::Summary(VersionSummary {
                evidence: Arc::new(evidence),
                version,
            }));
        }
        let virtual_assets = virtual_assets.ok_or(InstallError::NotReady)?;
        let client_jar = client_path.join_under(
            &evidence
                .pin
                .read_projection()
                .map_err(|_| InstallError::LibraryUnavailable)?,
        );
        let receipt = InstalledVersionReceipt {
            evidence: Arc::new(evidence),
            version,
            exact,
            virtual_assets,
            client_jar,
        };
        receipt.revalidate()?;
        Ok(VersionInspection::Ready(receipt))
    }
}

pub(crate) enum VersionInspection {
    Ready(InstalledVersionReceipt),
    Summary(VersionSummary),
    Damaged(ObservedDamage),
}

impl VersionInspection {
    pub(crate) fn into_ready(self) -> Result<InstalledVersionReceipt, super::queue::InstallError> {
        match self {
            Self::Ready(receipt) => Ok(receipt),
            Self::Summary(_) => Err(super::queue::InstallError::NotReady),
            Self::Damaged(damage) => Err(damage
                .reasons
                .first()
                .copied()
                .unwrap_or(super::queue::InstallError::NotReady)),
        }
    }
}

pub(crate) struct VersionSummary {
    pub(crate) version: VersionJson,
    pub(crate) evidence: Arc<InventoryEvidence>,
}

pub(crate) struct ObservedDamage {
    evidence: InventoryEvidence,
    // Distinct supported reasons, not an enumeration of every damaged file.
    reasons: Vec<super::queue::InstallError>,
}

impl ObservedDamage {
    pub(crate) fn retain_inventory(
        self,
    ) -> Result<Arc<InventoryEvidence>, super::queue::InstallError> {
        InventoryEvidence::retain(&mut Arc::new(self.evidence))
    }

    pub(crate) fn reasons(&self) -> &[super::queue::InstallError] {
        &self.reasons
    }

    pub(crate) fn entry_count(&self) -> u64 {
        self.evidence.entry_count()
    }

    pub(crate) fn revalidate(&self) -> Result<(), super::queue::InstallError> {
        self.evidence.revalidate()
    }
}

pub(crate) struct InventoryEvidence {
    pin: GenerationPin,
    operation: ManagedLibraryOperation,
    guards: Vec<(PortableRelativePath, axial_fs::FileRevisionObservation)>,
    missing: Vec<FileAbsence>,
    scratch: Option<axial_resource::PhysicalScratchPermit>,
}

impl InventoryEvidence {
    pub(crate) fn retain(
        evidence: &mut Arc<Self>,
    ) -> Result<Arc<Self>, super::queue::InstallError> {
        use super::queue::InstallError;
        if evidence.scratch.is_some() {
            return Ok(evidence.clone());
        }
        let retained = Arc::get_mut(evidence).ok_or(InstallError::NotReady)?;
        // Shared holders only, not transient decoding or shared native authority.
        let missing_bytes = retained.missing.iter().try_fold(0_u64, |bytes, absence| {
            bytes
                .checked_add(
                    absence
                        .retained_storage_bytes()
                        .map_err(|_| InstallError::AtCapacity)?,
                )
                .ok_or(InstallError::AtCapacity)
        })?;
        let missing_spare = ((retained.missing.capacity() - retained.missing.len()) as u64)
            .checked_mul(std::mem::size_of::<FileAbsence>() as u64)
            .ok_or(InstallError::AtCapacity)?;
        let path_bytes = (retained.guards.len() as u64)
            .checked_mul(axial_minecraft::portable_path::MAX_PORTABLE_RELATIVE_PATH_BYTES as u64)
            .ok_or(InstallError::AtCapacity)?;
        let bytes = (retained.guards.capacity() as u64)
            .checked_mul(std::mem::size_of::<(
                PortableRelativePath,
                axial_fs::FileRevisionObservation,
            )>() as u64)
            .and_then(|bytes| bytes.checked_add(path_bytes))
            .and_then(|bytes| bytes.checked_add(missing_bytes))
            .and_then(|bytes| bytes.checked_add(missing_spare))
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<InventoryEvidence>() as u64))
            .and_then(|bytes| bytes.checked_add(1024))
            .ok_or(InstallError::AtCapacity)?;
        retained.scratch = axial_resource::process_physical_work()
            .try_reserve_scratch(bytes)
            .map_err(|_| InstallError::AtCapacity)?;
        Ok(evidence.clone())
    }

    pub(crate) fn entry_count(&self) -> u64 {
        self.guards.len() as u64 + self.missing.len() as u64
    }

    pub(crate) fn revalidate(&self) -> Result<(), super::queue::InstallError> {
        use super::queue::InstallError;
        self.pin.revalidate().map_err(|_| InstallError::NotReady)?;
        let mut batch = self.operation.file_batch();
        for (path, expected) in &self.guards {
            batch
                .validate_revision(path, expected)
                .map_err(|_| InstallError::NotReady)?;
        }
        for absence in &self.missing {
            absence.revalidate().map_err(|_| InstallError::NotReady)?;
        }
        Ok(())
    }
}

/// Verified installation inputs kept alive until the game and output streams settle.
pub struct InstalledVersionReceipt {
    evidence: Arc<InventoryEvidence>,
    version: VersionJson,
    exact: BTreeMap<String, (String, u64)>,
    virtual_assets: bool,
    client_jar: PathBuf,
}

#[derive(Debug)]
pub(crate) enum GameLibrariesError {
    Install(super::queue::InstallError),
    Physical(axial_resource::PhysicalWorkError),
}

pub(crate) struct GameLibraries {
    pub requirements: axial_minecraft::loaders::game_libraries::Requirements,
    pub sources: Vec<axial_minecraft::managed_path::ManagedLibraryFile>,
}

impl std::fmt::Debug for InstalledVersionReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstalledVersionReceipt")
            .field("version", &self.version.id)
            .finish_non_exhaustive()
    }
}

impl InstalledVersionReceipt {
    pub(crate) fn retain_inventory(
        &mut self,
    ) -> Result<Arc<InventoryEvidence>, super::queue::InstallError> {
        if !self.evidence.missing.is_empty() {
            return Err(super::queue::InstallError::NotReady);
        }
        InventoryEvidence::retain(&mut self.evidence)
    }

    pub fn version(&self) -> &VersionJson {
        &self.version
    }
    pub fn virtual_assets(&self) -> bool {
        self.virtual_assets
    }
    pub fn client_jar(&self) -> &Path {
        &self.client_jar
    }

    pub fn revalidate(&self) -> Result<(), super::queue::InstallError> {
        self.evidence.revalidate()
    }

    pub(crate) async fn prepare_game_libraries(
        self,
        budget: Option<Arc<std::sync::Mutex<InventoryBudget>>>,
    ) -> Result<(Self, Option<GameLibraries>), GameLibrariesError> {
        use super::queue::InstallError;
        use axial_minecraft::loaders::game_libraries;
        use axial_resource::{PhysicalIoClass, PhysicalWorkRequest, process_physical_work};

        if !game_libraries::applies_to(&self.version.id) {
            return Ok((self, None));
        }
        let client_path = format!("versions/{0}/{0}.jar", self.version.id);
        let size = self
            .exact
            .get(&client_path)
            .map(|(_, size)| *size)
            .ok_or(GameLibrariesError::Install(InstallError::NotReady))?;
        let scratch = game_libraries::scratch_bytes(size)
            .map_err(|_| GameLibrariesError::Install(InstallError::NotReady))?;
        if let Some(budget) = &budget {
            let bytes = size
                .checked_add(2)
                .ok_or(GameLibrariesError::Install(InstallError::AtCapacity))?;
            budget
                .lock()
                .unwrap()
                .reserve(1, bytes)
                .map_err(GameLibrariesError::Install)?;
        }
        let work = process_physical_work();
        let retained_scratch = if budget.is_some() {
            work.try_reserve_scratch(scratch)
                .map_err(GameLibrariesError::Physical)?
        } else {
            None
        };
        work.admit(PhysicalWorkRequest::foreground(
            PhysicalIoClass::Read,
            if budget.is_some() { 0 } else { scratch },
        ))
        .await
        .map_err(GameLibrariesError::Physical)?
        .run(move |_| {
            let _scratch = retained_scratch;
            let path = PortableRelativePath::new_exact(&client_path)
                .map_err(|_| InstallError::NotReady)?;
            let expected = self
                .evidence
                .guards
                .iter()
                .find(|(recorded, _)| recorded == &path)
                .map(|(_, revision)| revision)
                .ok_or(InstallError::NotReady)?;
            let client = self
                .evidence
                .operation
                .observe_file(&path)
                .map_err(|_| InstallError::NotReady)?
                .ok_or(InstallError::NotReady)?;
            if client.revision_observation() != *expected {
                return Err(InstallError::NotReady);
            }
            let bytes = client
                .read_bounded(size)
                .map_err(|_| InstallError::NotReady)?;
            let requirements = game_libraries::recognize(&self.version.id, &bytes)
                .map_err(|_| InstallError::NotReady)?;
            if let Some(budget) = &budget {
                let checks = requirements
                    .as_ref()
                    .map_or(0, |requirements| requirements.entries().len() as u64)
                    .checked_add(self.evidence.entry_count())
                    .ok_or(InstallError::AtCapacity)?;
                budget.lock().unwrap().reserve_checks(checks)?;
            }
            let mut sources = Vec::new();
            if let Some(requirements) = &requirements {
                for entry in requirements.entries() {
                    let path = format!("libraries/{}", entry.path());
                    if !self
                        .exact
                        .get(&path)
                        .is_some_and(|(sha1, size)| sha1 == entry.sha1() && *size == entry.size())
                    {
                        return Err(InstallError::NotReady);
                    }
                    let path = PortableRelativePath::new_exact(&path)
                        .map_err(|_| InstallError::NotReady)?;
                    let expected = self
                        .evidence
                        .guards
                        .iter()
                        .find(|(recorded, _)| recorded == &path)
                        .map(|(_, revision)| revision)
                        .ok_or(InstallError::NotReady)?;
                    let source = self
                        .evidence
                        .operation
                        .observe_file(&path)
                        .map_err(|_| InstallError::NotReady)?
                        .ok_or(InstallError::NotReady)?;
                    if source.revision_observation() != *expected {
                        return Err(InstallError::NotReady);
                    }
                    sources.push(source);
                }
            }
            self.revalidate()?;
            Ok((
                self,
                requirements.map(|requirements| GameLibraries {
                    requirements,
                    sources,
                }),
            ))
        })
        .await
        .map_err(GameLibrariesError::Physical)?
        .map_err(GameLibrariesError::Install)
    }

    pub(crate) async fn prepare_natives(
        &self,
        library: &ManagedLibraryOperation,
        root: &Path,
        environment: &Environment,
    ) -> Result<Option<super::vanilla::PreparedNatives>, super::vanilla::NativePreparationError>
    {
        self.evidence
            .operation
            .validate_read_projection(root)
            .map_err(|_| super::vanilla::NativePreparationError::Changed)?;
        super::vanilla::prepare_natives_with_exact_files(
            library,
            root,
            &self.version,
            environment,
            self.exact.clone(),
        )
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::queue::InstallError;
    use super::*;
    use crate::library::{LibraryLifecycle, LibraryOpenOutcome};
    use sha1::{Digest, Sha1};

    fn fixture() -> (tempfile::TempDir, LibraryLifecycle, ActivatedVersion) {
        let temporary =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let mut files = Vec::new();
        for (path, bytes) in [
            (
                "versions/1.21.4/1.21.4.json",
                &br#"{"id":"1.21.4","type":"release"}"#[..],
            ),
            ("assets/objects/aa/second", &b"second payload"[..]),
            ("versions/1.21.4/1.21.4.jar", &b"fixture client"[..]),
            ("assets/objects/aa/first", &b"first payload"[..]),
        ] {
            let target = temporary.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, bytes).unwrap();
            files.push(ActivatedFile {
                path: path.into(),
                sha1: hex::encode(Sha1::digest(bytes)),
                size: bytes.len() as u64,
            });
        }
        let LibraryOpenOutcome::Ready(library) = LibraryLifecycle::open(temporary.path()) else {
            panic!("isolated library admission");
        };
        (
            temporary,
            library,
            ActivatedVersion {
                version_id: "1.21.4".into(),
                contract_id: format!("managed-install-activation-v1.{}", "A".repeat(43)),
                files,
            },
        )
    }

    pub(crate) async fn preparation_child(selector: &str) -> bool {
        use std::time::{Duration, Instant};

        const CHILD: &str = "AXIAL_INVENTORY_PREPARATION_CHILD";
        if std::env::var(CHILD).ok().as_deref() == Some(selector) {
            return true;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", selector, "--nocapture"])
            .env(CHILD, selector)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let observed = loop {
            match child.try_wait() {
                Ok(None) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => break result,
            }
        };
        if !matches!(&observed, Ok(Some(_))) {
            let _ = child.kill();
        }
        let joined = child.wait();
        assert!(
            matches!(&observed, Ok(Some(status)) if status.success())
                && joined.as_ref().is_ok_and(|status| status.success()),
            "isolated preparation control: observed={observed:?}, joined={joined:?}",
        );
        false
    }

    #[tokio::test]
    async fn recorded_inventory_preserves_entry_limit_during_decode() {
        if !preparation_child(
            "install::artifacts::tests::recorded_inventory_preserves_entry_limit_during_decode",
        )
        .await
        {
            return;
        }
        let entry = r#"{"path":"","sha1":"","size":0}"#;
        let mut record = String::with_capacity(1_000_000 * (entry.len() + 1) + 100);
        record.push_str(r#"{"version_id":"fixture","contract_id":"fixture","files":["#);
        for index in 0..1_000_000 {
            if index != 0 {
                record.push(',');
            }
            record.push_str(entry);
        }
        record.push_str("]}");
        let accepted: ActivatedVersion = serde_json::from_str(&record).unwrap();
        assert_eq!(accepted.files.len(), 1_000_000);
        drop(accepted);
        record.truncate(record.len() - 2);
        record.push(',');
        record.push_str(entry);
        record.push_str("]}");
        assert!(
            serde_json::from_str::<ActivatedVersion>(&record).is_err(),
            "overflow must refuse during decoding, not after allocating every entry"
        );
    }

    #[test]
    fn recorded_inventory_rejects_malformed_file_records() {
        for file in [
            r#"{"path":[],"sha1":"","size":0}"#,
            r#"{"path":"","path":"other","sha1":"","size":0}"#,
            r#"{"path":"","sha1":"","size":0,"extra":true}"#,
        ] {
            let record =
                format!(r#"{{"version_id":"fixture","contract_id":"fixture","files":[{file}]}}"#);
            assert!(serde_json::from_str::<ActivatedVersion>(&record).is_err());
        }
    }

    fn historical_fixture() -> (tempfile::TempDir, LibraryLifecycle, ActivatedVersion) {
        let temporary =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let version_id = axial_minecraft::loaders::installed_version_id_for(
            axial_minecraft::LoaderComponentId::Forge,
            "1.4.7",
            "6.6.2.534",
        )
        .unwrap();
        let metadata = serde_json::to_vec(&serde_json::json!({
            "id": version_id, "type": "release",
        }))
        .unwrap();
        // A valid empty ZIP reaches historical preparation without an FML declaration.
        let client = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        let mut files = Vec::new();
        for (extension, bytes) in [("json", metadata.as_slice()), ("jar", client.as_slice())] {
            let path = format!("versions/{version_id}/{version_id}.{extension}");
            let target = temporary.path().join(&path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, bytes).unwrap();
            files.push(ActivatedFile {
                path,
                sha1: hex::encode(Sha1::digest(bytes)),
                size: bytes.len() as u64,
            });
        }
        let LibraryOpenOutcome::Ready(library) = LibraryLifecycle::open(temporary.path()) else {
            panic!("isolated library admission");
        };
        let activated = ActivatedVersion {
            version_id,
            contract_id: format!("managed-install-activation-v1.{}", "A".repeat(43)),
            files,
        };
        (temporary, library, activated)
    }

    #[test]
    fn inventory_budget_includes_checksum_and_json_growth_probes() {
        let (_temporary, library, activated) = fixture();
        let mut budget = InventoryBudget::projection();
        // Four payloads total 73 bytes; checksum probes add 8, metadata reread 34.
        budget.reserve_record((16_u64 << 30) - 115).unwrap();
        budget
            .reserve_inventory(&activated, Inspection::Integrity)
            .unwrap();
        let receipt = activated.verify(library.admit().unwrap()).unwrap();
        receipt.revalidate().unwrap();
        drop(receipt);
        assert!(matches!(
            budget.reserve_record(1),
            Err(InstallError::AtCapacity)
        ));
    }

    #[tokio::test]
    async fn grouped_historical_preparation_refuses_retained_scratch_pressure() {
        use axial_resource::{PhysicalWorkClass, PhysicalWorkError, process_physical_work};
        use std::time::Duration;

        if !preparation_child("install::artifacts::tests::grouped_historical_preparation_refuses_retained_scratch_pressure").await {
            return;
        }
        let (temporary, library, activated) = historical_fixture();
        let version_id = activated.version_id.clone();
        let metadata = std::fs::read(
            temporary
                .path()
                .join(format!("versions/{version_id}/{version_id}.json")),
        )
        .unwrap();
        let client = std::fs::read(
            temporary
                .path()
                .join(format!("versions/{version_id}/{version_id}.jar")),
        )
        .unwrap();
        let pin = library.admit().unwrap();
        let mut receipt = activated.clone().verify(pin.clone()).unwrap();
        let evidence = receipt.retain_inventory().unwrap();
        let original_valid = evidence.revalidate().is_ok();
        let work = process_physical_work();
        let remaining = work
            .snapshot(PhysicalWorkClass::Foreground)
            .available_scratch_bytes;
        let occupied = work.try_reserve_scratch(remaining);
        let pressure_admitted = matches!(&occupied, Ok(Some(_)));
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            receipt.prepare_game_libraries(Some(Arc::new(std::sync::Mutex::new(
                InventoryBudget::projection(),
            )))),
        )
        .await;
        let refused = matches!(
            &result,
            Ok(Err(GameLibrariesError::Physical(
                PhysicalWorkError::Unavailable
            )))
        );
        drop(result);
        let before_release = work.snapshot(PhysicalWorkClass::Foreground);
        drop(occupied);
        let preserved = evidence.revalidate().is_ok();
        let positive = match activated.verify(pin.clone()) {
            Ok(receipt) => receipt.prepare_game_libraries(None).await,
            Err(error) => Err(GameLibrariesError::Install(error)),
        };
        let prepared = matches!(&positive, Ok((_, None)));
        drop(positive);
        let unchanged = std::fs::read(
            temporary
                .path()
                .join(format!("versions/{version_id}/{version_id}.json")),
        )
        .is_ok_and(|bytes| bytes == metadata)
            && std::fs::read(
                temporary
                    .path()
                    .join(format!("versions/{version_id}/{version_id}.jar")),
            )
            .is_ok_and(|bytes| bytes == client);
        drop((evidence, pin));
        let settled = library.try_preserve();
        drop(library);
        let idle = work.snapshot(PhysicalWorkClass::Foreground);
        if !original_valid
            || !pressure_admitted
            || !refused
            || !preserved
            || !prepared
            || !unchanged
            || settled.is_err()
            || before_release.active_admissions != 0
            || before_release.running_workers != 0
            || before_release.available_scratch_bytes != 0
            || idle.active_admissions != 0
            || idle.running_workers != 0
            || idle.available_scratch_bytes != work.scratch_limit_bytes()
        {
            eprintln!(
                "scratch control fixture retained at {}",
                temporary.keep().display()
            );
        }
        assert!(
            original_valid
                && pressure_admitted
                && preserved
                && prepared
                && unchanged
                && settled.is_ok()
        );
        assert_eq!(before_release.active_admissions, 0);
        assert_eq!(before_release.running_workers, 0);
        assert_eq!(before_release.available_scratch_bytes, 0);
        assert_eq!(idle.active_admissions, 0);
        assert_eq!(idle.running_workers, 0);
        assert_eq!(idle.available_scratch_bytes, work.scratch_limit_bytes());
        assert!(
            refused,
            "grouped preparation must refuse rather than await retained scratch"
        );
    }

    #[tokio::test]
    async fn grouped_historical_preparation_reserves_reads_and_revision_passes() {
        use axial_resource::{PhysicalWorkClass, process_physical_work};

        if !preparation_child("install::artifacts::tests::grouped_historical_preparation_reserves_reads_and_revision_passes").await {
            return;
        }
        let (temporary, library, activated) = historical_fixture();
        let version_id = &activated.version_id;
        let paths = [
            temporary
                .path()
                .join(format!("versions/{version_id}/{version_id}.json")),
            temporary
                .path()
                .join(format!("versions/{version_id}/{version_id}.jar")),
        ];
        let before = paths.each_ref().map(|path| std::fs::read(path).unwrap());
        let pin = library.admit().unwrap();
        let original = activated.clone().verify(pin.clone()).unwrap();
        let positive = match activated.clone().verify(pin.clone()) {
            Ok(receipt) => {
                receipt
                    .prepare_game_libraries(Some(Arc::new(std::sync::Mutex::new(
                        InventoryBudget::projection(),
                    ))))
                    .await
            }
            Err(error) => Err(GameLibrariesError::Install(error)),
        };
        let prepared = matches!(&positive, Ok((_, None)));
        drop(positive);

        let mut observations = Vec::new();
        for (name, checks, bytes) in [
            ("client observation", 0, 24),
            ("client read and probes", 3, 23),
            ("final original revisions", 2, 24),
            ("exact allowance", 3, 24),
        ] {
            let mut budget = InventoryBudget::projection();
            budget.reserve_checks(1_000_000 - checks).unwrap();
            budget.reserve_record((16_u64 << 30) - bytes).unwrap();
            let result = match activated.clone().verify(pin.clone()) {
                Ok(receipt) => {
                    receipt
                        .prepare_game_libraries(Some(Arc::new(std::sync::Mutex::new(budget))))
                        .await
                }
                Err(error) => Err(GameLibrariesError::Install(error)),
            };
            observations.push((
                name,
                matches!(
                    &result,
                    Err(GameLibrariesError::Install(InstallError::AtCapacity))
                ),
                matches!(&result, Ok((_, None))),
            ));
            drop(result);
        }
        let preserved = original.revalidate().is_ok();
        let unchanged = paths
            .iter()
            .zip(&before)
            .all(|(path, expected)| std::fs::read(path).is_ok_and(|bytes| bytes == *expected));
        drop((original, pin));
        let settled = library.try_preserve();
        drop(library);
        let work = process_physical_work();
        let idle = work.snapshot(PhysicalWorkClass::Foreground);
        let expected = [
            ("client observation", true, false),
            ("client read and probes", true, false),
            ("final original revisions", true, false),
            ("exact allowance", false, true),
        ];
        if !prepared
            || !preserved
            || !unchanged
            || settled.is_err()
            || idle.active_admissions != 0
            || idle.running_workers != 0
            || idle.available_scratch_bytes != work.scratch_limit_bytes()
            || observations != expected
        {
            eprintln!(
                "preparation budget fixture retained at {}",
                temporary.keep().display()
            );
        }
        assert!(prepared && preserved && unchanged && settled.is_ok());
        assert_eq!(idle.active_admissions, 0);
        assert_eq!(idle.running_workers, 0);
        assert_eq!(idle.available_scratch_bytes, work.scratch_limit_bytes());
        assert_eq!(observations, expected);
    }

    #[tokio::test]
    async fn repeated_inventory_retention_shares_charge_until_last_reader_drops() {
        use axial_resource::{PhysicalWorkClass, process_physical_work};

        if !preparation_child("install::artifacts::tests::repeated_inventory_retention_shares_charge_until_last_reader_drops").await {
            return;
        }
        let (temporary, library, activated) = historical_fixture();
        let client = temporary
            .path()
            .join(format!("versions/{0}/{0}.jar", activated.version_id,));
        let original_bytes = std::fs::read(&client).unwrap();
        let pin = library.admit().unwrap();
        let mut receipt = activated.clone().verify(pin.clone()).unwrap();
        let work = process_physical_work();
        let before = work
            .snapshot(PhysicalWorkClass::Foreground)
            .available_scratch_bytes;
        let first = receipt.retain_inventory().unwrap();
        let once = work
            .snapshot(PhysicalWorkClass::Foreground)
            .available_scratch_bytes;
        let second = receipt.retain_inventory().unwrap();
        let twice = work
            .snapshot(PhysicalWorkClass::Foreground)
            .available_scratch_bytes;
        let valid = first.revalidate().is_ok() && second.revalidate().is_ok();
        drop(receipt);
        let receipt_dropped = work
            .snapshot(PhysicalWorkClass::Foreground)
            .available_scratch_bytes;
        drop(first);
        let first_dropped = work
            .snapshot(PhysicalWorkClass::Foreground)
            .available_scratch_bytes;

        let previous = client.with_file_name("previous-client.jar");
        let replacement = std::fs::rename(&client, &previous)
            .and_then(|()| std::fs::write(&client, &original_bytes));
        let refused = matches!(second.revalidate(), Err(InstallError::NotReady));
        let fresh = activated.verify(pin.clone());
        let fresh_valid = fresh
            .as_ref()
            .is_ok_and(|receipt| receipt.revalidate().is_ok());
        drop(fresh);
        let restored = if replacement.is_ok() {
            std::fs::remove_file(&client).and_then(|()| std::fs::rename(&previous, &client))
        } else {
            Err(std::io::Error::other("fixture replacement failed"))
        };
        let unchanged = std::fs::read(&client).is_ok_and(|bytes| bytes == original_bytes);
        drop((second, pin));
        let settled = library.try_preserve();
        drop(library);
        let idle = work.snapshot(PhysicalWorkClass::Foreground);
        let charges = [twice, receipt_dropped, first_dropped];
        if !valid
            || replacement.is_err()
            || !refused
            || !fresh_valid
            || restored.is_err()
            || !unchanged
            || settled.is_err()
            || once >= before
            || charges != [once; 3]
            || idle.available_scratch_bytes != before
            || idle.active_admissions != 0
            || idle.running_workers != 0
        {
            eprintln!(
                "inventory retention fixture retained at {}",
                temporary.keep().display()
            );
        }
        assert!(
            valid
                && replacement.is_ok()
                && refused
                && fresh_valid
                && restored.is_ok()
                && unchanged
                && settled.is_ok()
        );
        assert_eq!(idle.active_admissions, 0);
        assert_eq!(idle.running_workers, 0);
        assert_eq!(idle.available_scratch_bytes, before);
        assert!(once < before);
        assert_eq!(
            charges, [once; 3],
            "one charge survives until the last reader"
        );
    }

    #[test]
    fn file_batch_inventory_keeps_hash_duplicate_and_final_revision_checks() {
        let (temporary, library, activated) = fixture();
        let pin = library.admit().unwrap();
        let receipt = activated.clone().verify(pin.clone()).unwrap();
        receipt.revalidate().unwrap();
        let mut duplicate = activated.clone();
        duplicate.files.push(duplicate.files[1].clone());
        assert!(matches!(
            duplicate.verify(pin.clone()),
            Err(super::super::queue::InstallError::NotReady)
        ));
        std::fs::write(
            temporary.path().join("assets/objects/aa/first"),
            b"other payload",
        )
        .unwrap();
        assert!(receipt.revalidate().is_err());
        assert!(activated.verify(pin).is_err());
    }

    #[test]
    fn summary_observes_metadata_but_never_confers_integrity() {
        let (temporary, library, mut activated) = fixture();
        for (path, bytes) in [
            ("libraries/fixture.jar", &b"library"[..]),
            ("assets/indexes/fixture.json", &br#"{"objects":{}}"#[..]),
        ] {
            let target = temporary.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, bytes).unwrap();
            activated.files.push(ActivatedFile {
                path: path.into(),
                sha1: hex::encode(Sha1::digest(bytes)),
                size: bytes.len() as u64,
            });
        }
        let pin = library.admit().unwrap();
        activated.clone().verify(pin.clone()).unwrap();
        assert!(
            activated
                .clone()
                .inspect(pin.clone(), Inspection::Summary, true)
                .unwrap()
                .into_ready()
                .is_err()
        );
        for (path, missing, ready) in [
            ("versions/1.21.4/1.21.4.jar", false, true),
            ("libraries/fixture.jar", false, true),
            ("assets/indexes/fixture.json", false, true),
            ("assets/objects/aa/first", true, true),
            ("libraries/fixture.jar", true, false),
            ("assets/indexes/fixture.json", true, false),
        ] {
            let target = temporary.path().join(path);
            let original = std::fs::read(&target).unwrap();
            if missing {
                std::fs::remove_file(&target).unwrap();
            } else {
                std::fs::write(&target, vec![b'x'; original.len()]).unwrap();
            }
            let observed = activated
                .clone()
                .inspect(pin.clone(), Inspection::Summary, true)
                .unwrap();
            let strict_refused = activated.clone().verify(pin.clone()).is_err();
            let summary_ready = match &observed {
                VersionInspection::Summary(summary) => {
                    summary.evidence.revalidate().unwrap();
                    true
                }
                VersionInspection::Damaged(damage) => {
                    damage.revalidate().unwrap();
                    false
                }
                VersionInspection::Ready(_) => panic!("summary produced launch authority"),
            };
            std::fs::write(&target, original).unwrap();
            activated.clone().verify(pin.clone()).unwrap();
            assert_eq!(summary_ready, ready, "{path}, missing={missing}");
            assert!(strict_refused, "{path}, missing={missing}");
        }
    }

    #[test]
    fn summary_validates_skipped_records_and_reserves_only_observed_reads() {
        let (_temporary, library, activated) = fixture();
        let pin = library.admit().unwrap();
        let mut budget = InventoryBudget::projection();
        // Four record entries, two observations, and two 32-byte metadata reads plus probes.
        budget.reserve_checks(1_000_000 - 6).unwrap();
        budget.reserve_record((16_u64 << 30) - 68).unwrap();
        budget
            .reserve_inventory(&activated, Inspection::Summary)
            .unwrap();
        assert!(budget.reserve_checks(1).is_err());
        assert!(budget.reserve_record(1).is_err());
        for malformed in ["digest", "duplicate", "path"] {
            let mut changed = activated.clone();
            let asset = changed
                .files
                .iter_mut()
                .find(|file| file.path.ends_with("/second"))
                .unwrap();
            match malformed {
                "digest" => asset.sha1 = "X".repeat(40),
                "path" => asset.path = "assets/objects/../second".into(),
                _ => {
                    let duplicate = asset.clone();
                    changed.files.push(duplicate);
                }
            }
            assert!(
                changed
                    .inspect(pin.clone(), Inspection::Summary, true)
                    .is_err(),
                "{malformed}"
            );
        }
    }

    #[tokio::test]
    async fn negative_inventory_retention_charges_unused_capacity() {
        if !preparation_child(
            "install::artifacts::tests::negative_inventory_retention_charges_unused_capacity",
        )
        .await
        {
            return;
        }
        let (temporary, library, mut activated) = fixture();
        for index in 0..64 {
            activated.files.push(ActivatedFile {
                path: format!("libraries/missing-{index}.jar"),
                sha1: "a".repeat(40),
                size: 1,
            });
        }
        let VersionInspection::Damaged(damage) = activated
            .inspect(library.admit().unwrap(), Inspection::Summary, true)
            .unwrap()
        else {
            panic!("missing libraries must retain their observation");
        };
        let work = axial_resource::process_physical_work();
        let class = axial_resource::PhysicalWorkClass::Foreground;
        let before = work.snapshot(class).available_scratch_bytes;
        let evidence = damage.retain_inventory().unwrap();
        let charge = before - work.snapshot(class).available_scratch_bytes;
        let observed = evidence.revalidate();
        let minimum = 66
            * std::mem::size_of::<(PortableRelativePath, axial_fs::FileRevisionObservation)>()
                as u64
            + 2 * axial_minecraft::portable_path::MAX_PORTABLE_RELATIVE_PATH_BYTES as u64
            + std::mem::size_of::<InventoryEvidence>() as u64
            + 1024;
        drop(evidence);
        let restored = work.snapshot(class).available_scratch_bytes == before;
        let settled = library.try_preserve();
        if settled.is_err() {
            std::mem::forget(library);
            eprintln!(
                "negative retention retained: {}",
                temporary.keep().display()
            );
        }
        settled.unwrap();
        observed.unwrap();
        assert!(charge >= minimum, "charge={charge}, minimum={minimum}");
        assert!(restored);
    }

    #[test]
    fn missing_client_observation_refuses_restored_file() {
        let (temporary, library, activated) = fixture();
        let pin = library.admit().unwrap();
        activated.clone().verify(pin.clone()).unwrap();
        let client = temporary.path().join("versions/1.21.4/1.21.4.jar");
        let original = std::fs::read(&client).unwrap();
        std::fs::remove_file(&client).unwrap();
        let VersionInspection::Damaged(damage) = activated
            .clone()
            .inspect(pin.clone(), Inspection::Integrity, true)
            .unwrap()
        else {
            panic!("missing client must retain observed damage");
        };
        assert_eq!(damage.reasons(), &[InstallError::ClientJarMissing]);
        damage.revalidate().unwrap();

        std::fs::write(&client, &original).unwrap();
        assert!(matches!(damage.revalidate(), Err(InstallError::NotReady)));
        let VersionInspection::Ready(receipt) =
            activated.inspect(pin, Inspection::Integrity, true).unwrap()
        else {
            panic!("fresh inspection must admit the restored client");
        };
        receipt.revalidate().unwrap();
        assert_eq!(std::fs::read(client).unwrap(), original);
    }

    #[test]
    fn corrupt_client_observation_refuses_same_byte_replacement() {
        let (temporary, library, activated) = fixture();
        let pin = library.admit().unwrap();
        activated.clone().verify(pin.clone()).unwrap();
        let client = temporary.path().join("versions/1.21.4/1.21.4.jar");
        let corrupt = b"corrupt client";
        assert_eq!(std::fs::read(&client).unwrap().len(), corrupt.len());
        std::fs::write(&client, corrupt).unwrap();
        let VersionInspection::Damaged(damage) = activated
            .clone()
            .inspect(pin.clone(), Inspection::Integrity, true)
            .unwrap()
        else {
            panic!("corrupt client must retain observed damage");
        };
        assert_eq!(damage.reasons(), &[InstallError::ClientJarCorrupt]);
        damage.revalidate().unwrap();

        let previous = client.with_file_name("previous-client.jar");
        std::fs::rename(&client, &previous).unwrap();
        std::fs::write(&client, corrupt).unwrap();
        assert!(matches!(damage.revalidate(), Err(InstallError::NotReady)));
        let VersionInspection::Damaged(fresh) =
            activated.inspect(pin, Inspection::Integrity, true).unwrap()
        else {
            panic!("replacement client must remain corrupt");
        };
        assert_eq!(fresh.reasons(), &[InstallError::ClientJarCorrupt]);
        fresh.revalidate().unwrap();
        assert_eq!(std::fs::read(previous).unwrap(), corrupt);
        assert_eq!(std::fs::read(client).unwrap(), corrupt);
    }

    #[test]
    fn diagnostic_inspection_refuses_later_malformed_inventory() {
        let (temporary, library, mut activated) = fixture();
        let pin = library.admit().unwrap();
        activated.clone().verify(pin.clone()).unwrap();
        std::fs::remove_file(temporary.path().join("versions/1.21.4/1.21.4.jar")).unwrap();
        activated.files.push(ActivatedFile {
            path: "versions/z/z.jar".into(),
            sha1: "x".repeat(40),
            size: 0,
        });
        assert!(matches!(
            activated
                .clone()
                .inspect(pin.clone(), Inspection::Integrity, false),
            Err(InstallError::ClientJarMissing)
        ));
        assert!(matches!(
            activated.inspect(pin, Inspection::Integrity, true),
            Err(InstallError::NotReady)
        ));
    }
}

#[must_use = "publication must be activated, acknowledged, or retained for settlement"]
pub enum InstallReceiptState {
    AwaitingActivation {
        verified: VerifiedManagedInstallReceipt<KnownGoodInstallReceipt>,
        evidence: String,
    },
    NoEffect(KnownGoodInstallReceipt),
    Mismatch(KnownGoodInstallReceipt),
    ReceiptMismatch(ManagedInstallReceiptVerificationFailure<KnownGoodInstallReceipt>),
    RolledBack {
        receipt: KnownGoodInstallReceipt,
        evidence: ManagedInstallRolledBackEvidence,
        effect: ManagedInstallRollbackEffect,
    },
    Indeterminate {
        receipt: KnownGoodInstallReceipt,
        recovery: ManagedInstallDurableRecovery,
    },
}

pub async fn inspect_install_receipt(
    library: ManagedLibraryOperation,
    receipt: KnownGoodInstallReceipt,
) -> InstallReceiptState {
    let outcome = axial_minecraft::classify_managed_install_publication(
        library,
        receipt.version_id().to_string(),
    )
    .await;
    classify_receipt(receipt, outcome)
}

pub async fn resume_install_receipt(
    receipt: KnownGoodInstallReceipt,
    recovery: ManagedInstallDurableRecovery,
) -> InstallReceiptState {
    classify_receipt(receipt, recovery.retry().await)
}

fn classify_receipt(
    receipt: KnownGoodInstallReceipt,
    outcome: ManagedInstallDurableOutcome,
) -> InstallReceiptState {
    match outcome {
        ManagedInstallDurableOutcome::Committed(evidence) => {
            let id = evidence.id().as_str().to_owned();
            match evidence.verify_install_receipt(receipt) {
                Ok(verified) => InstallReceiptState::AwaitingActivation {
                    verified,
                    evidence: id,
                },
                Err(failure) => InstallReceiptState::ReceiptMismatch(failure),
            }
        }
        ManagedInstallDurableOutcome::NoEffect => InstallReceiptState::NoEffect(receipt),
        ManagedInstallDurableOutcome::Mismatch => InstallReceiptState::Mismatch(receipt),
        ManagedInstallDurableOutcome::RolledBack { evidence, effect } => {
            InstallReceiptState::RolledBack {
                receipt,
                evidence,
                effect,
            }
        }
        ManagedInstallDurableOutcome::Indeterminate(recovery) => {
            InstallReceiptState::Indeterminate { receipt, recovery }
        }
    }
}

#[must_use = "loader base is a checkpoint, not a completed loader installation"]
pub enum LoaderBaseState {
    AwaitingActivation {
        verified: VerifiedLoaderInstallBaseCommit,
        evidence: String,
    },
    NoEffect(LoaderInstallBaseCommit),
    Mismatch(LoaderInstallBaseCommit),
    ReceiptMismatch(LoaderInstallBaseCommitVerificationFailure),
    RolledBack {
        commit: LoaderInstallBaseCommit,
        evidence: ManagedInstallRolledBackEvidence,
        effect: ManagedInstallRollbackEffect,
    },
    Indeterminate {
        commit: LoaderInstallBaseCommit,
        recovery: ManagedInstallDurableRecovery,
    },
}

pub async fn inspect_loader_base_commit(
    library: ManagedLibraryOperation,
    commit: LoaderInstallBaseCommit,
) -> LoaderBaseState {
    let outcome = axial_minecraft::classify_managed_install_publication(
        library,
        commit.base_version_id().to_string(),
    )
    .await;
    classify_base(commit, outcome)
}

pub async fn resume_loader_base_commit(
    commit: LoaderInstallBaseCommit,
    recovery: ManagedInstallDurableRecovery,
) -> LoaderBaseState {
    classify_base(commit, recovery.retry().await)
}

fn classify_base(
    commit: LoaderInstallBaseCommit,
    outcome: ManagedInstallDurableOutcome,
) -> LoaderBaseState {
    match outcome {
        ManagedInstallDurableOutcome::Committed(evidence) => verify_base(evidence, commit),
        ManagedInstallDurableOutcome::NoEffect => LoaderBaseState::NoEffect(commit),
        ManagedInstallDurableOutcome::Mismatch => LoaderBaseState::Mismatch(commit),
        ManagedInstallDurableOutcome::RolledBack { evidence, effect } => {
            LoaderBaseState::RolledBack {
                commit,
                evidence,
                effect,
            }
        }
        ManagedInstallDurableOutcome::Indeterminate(recovery) => {
            LoaderBaseState::Indeterminate { commit, recovery }
        }
    }
}

fn verify_base(
    evidence: ManagedInstallCommittedEvidence,
    commit: LoaderInstallBaseCommit,
) -> LoaderBaseState {
    let id = evidence.id().as_str().to_owned();
    match evidence.verify_loader_base_commit(commit) {
        Ok(verified) => LoaderBaseState::AwaitingActivation {
            verified,
            evidence: id,
        },
        Err(failure) => LoaderBaseState::ReceiptMismatch(failure),
    }
}
