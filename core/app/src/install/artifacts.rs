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
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::portable_path::PortableRelativePath;
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
};

/// Private durable projection, produced exclusively by verified activation. A
/// serialized record is never itself filesystem authority: every use observes
/// and verifies its exact files through the current retained generation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivatedVersion {
    pub version_id: String,
    pub contract_id: String,
    pub files: Vec<ActivatedFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivatedFile {
    pub path: String,
    pub sha1: String,
    pub size: u64,
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
        mut self,
        pin: GenerationPin,
    ) -> Result<InstalledVersionReceipt, super::queue::InstallError> {
        use super::queue::InstallError;
        axial_minecraft::ManagedInstallActivationContractId::parse(&self.contract_id)
            .map_err(|_| InstallError::NotReady)?;
        if self.files.is_empty() || self.files.len() > 1_000_000 {
            return Err(InstallError::NotReady);
        }
        let operation = pin
            .managed_library()
            .map_err(|_| InstallError::LibraryUnavailable)?;
        self.files
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let mut batch = operation.file_batch();
        let metadata_path = format!("versions/{0}/{0}.json", self.version_id);
        let mut guards = Vec::with_capacity(self.files.len());
        let mut version = None;
        let mut exact = BTreeMap::new();
        let mut asset_flags = BTreeMap::new();
        for expected in &self.files {
            if expected.size > 2 * 1024 * 1024 * 1024 || expected.sha1.len() != 40 {
                return Err(InstallError::NotReady);
            }
            let path = PortableRelativePath::new_exact(&expected.path)
                .map_err(|_| InstallError::NotReady)?;
            if !matches!(
                expected.path.split('/').next(),
                Some("versions" | "libraries" | "assets")
            ) || exact
                .insert(
                    expected.path.clone(),
                    (expected.sha1.clone(), expected.size),
                )
                .is_some()
            {
                return Err(InstallError::NotReady);
            }
            let file = batch
                .observe_file(&path)
                .map_err(|_| InstallError::NotReady)?
                .ok_or(InstallError::NotReady)?;
            if file.size() != expected.size
                || hex::encode(
                    file.sha1_bounded(expected.size)
                        .map_err(|_| InstallError::NotReady)?,
                ) != expected.sha1
            {
                return Err(InstallError::NotReady);
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
            if expected.path.starts_with("assets/indexes/") && expected.path.ends_with(".json") {
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
        let mut version = version.ok_or(InstallError::NotReady)?;
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
        let virtual_assets = if version.asset_index.id.is_empty() {
            false
        } else {
            *asset_flags
                .get(&format!("assets/indexes/{}.json", version.asset_index.id))
                .ok_or(InstallError::NotReady)?
        };
        let client = format!("versions/{0}/{0}.jar", version.id);
        if !exact.contains_key(&client) {
            return Err(InstallError::NotReady);
        }
        let client_jar = PortableRelativePath::new_exact(&client)
            .map_err(|_| InstallError::NotReady)?
            .join_under(
                &pin.read_projection()
                    .map_err(|_| InstallError::LibraryUnavailable)?,
            );
        let receipt = InstalledVersionReceipt {
            pin,
            operation,
            version,
            exact,
            guards,
            virtual_assets,
            client_jar,
        };
        receipt.revalidate()?;
        Ok(receipt)
    }
}

/// Verified installation inputs kept alive until the game and output streams settle.
pub struct InstalledVersionReceipt {
    pin: GenerationPin,
    operation: ManagedLibraryOperation,
    version: VersionJson,
    exact: BTreeMap<String, (String, u64)>,
    guards: Vec<(PortableRelativePath, axial_fs::FileRevisionObservation)>,
    virtual_assets: bool,
    client_jar: PathBuf,
}

impl std::fmt::Debug for InstalledVersionReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstalledVersionReceipt")
            .field("version", &self.version.id)
            .finish_non_exhaustive()
    }
}

impl InstalledVersionReceipt {
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
        self.pin
            .revalidate()
            .map_err(|_| super::queue::InstallError::NotReady)?;
        let mut batch = self.operation.file_batch();
        for (path, expected) in &self.guards {
            let file = batch
                .observe_file(path)
                .map_err(|_| super::queue::InstallError::NotReady)?
                .ok_or(super::queue::InstallError::NotReady)?;
            if file.revision_observation() != *expected {
                return Err(super::queue::InstallError::NotReady);
            }
        }
        Ok(())
    }

    pub(crate) async fn prepare_natives(
        &self,
        library: &ManagedLibraryOperation,
        root: &Path,
        environment: &Environment,
    ) -> Result<Option<super::vanilla::PreparedNatives>, super::vanilla::NativePreparationError>
    {
        self.operation
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
mod tests {
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
