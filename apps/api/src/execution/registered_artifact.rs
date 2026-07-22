//! Confined verification and mutation of exact registered launcher-managed artifacts.

use crate::execution::file::file_fact;
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::state::contracts::{OperationId, TargetDescriptor};
use axial_config::AppRootSession;
use axial_fs::{
    Directory, ExpectedFileContent, FileCapability, FileCreateObligation, FileCreateOutcome,
    FileCreateResolution, FileParkObligation, FileParkOutcome, FileParkPreservationError,
    FileParkResolution, FilePromotionObligation, FilePromotionOutcome, FilePromotionResolution,
    FileRevision, LeafName, ParkedFile, SealedStagedFile,
    StageDiscardObligation, StageDiscardOutcome, StageDiscardResolution, StagedFile,
};
use axial_minecraft::known_good::{KnownGoodPhysicalPath, MAX_TIER2_ARTIFACT_BYTES};
use futures_util::StreamExt;
use reqwest::Client;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use std::io::{self, Read as _, Write as _};
use std::path::Component;
use std::sync::Arc;

const DOWNLOAD_FRAME_BYTES: usize = 64 * 1024;
const DOWNLOAD_FRAME_CAPACITY: usize = 4;

#[derive(Clone)]
struct RegisteredArtifactLocation {
    parent: Directory,
    leaf: LeafName,
    root_session: Arc<AppRootSession>,
}

pub(crate) struct RegisteredArtifactMutationCapability {
    location: RegisteredArtifactLocation,
}

/// Fresh, read-only authority to verify one exact registered artifact leaf once.
pub(crate) struct RegisteredArtifactExactVerifier {
    location: RegisteredArtifactLocation,
    expected_sha1: String,
    expected_size: u64,
    identity: Arc<()>,
}

pub(crate) struct RegisteredArtifactExactVerification {
    identity: Arc<()>,
}

pub(crate) struct RegisteredArtifactExactProof {
    file: FileCapability,
    revision: FileRevision,
    _root_session: Arc<AppRootSession>,
    identity: Arc<()>,
    #[cfg(test)]
    lifetime: Arc<()>,
}

#[must_use = "mutation reports retain the exact published-file proof"]
pub(crate) struct RegisteredArtifactMutationReport {
    facts: Vec<ExecutionFact>,
    proof: RegisteredArtifactMutationProof,
}

#[must_use = "published-file proofs must be validated or retained through settlement"]
pub(crate) struct RegisteredArtifactMutationProof {
    file: FileCapability,
    revision: FileRevision,
    root_session: Arc<AppRootSession>,
}

#[must_use = "observed exact proofs must be validated or retained through settlement"]
pub(crate) struct RegisteredArtifactObservedExactProof {
    file: FileCapability,
    revision: FileRevision,
    _root_session: Arc<AppRootSession>,
}

pub(crate) struct RegisteredArtifactObservedExactValidationError {
    source: io::Error,
}

#[must_use = "a quarantine receipt must remain alive through its durable checkpoint"]
pub(crate) struct RegisteredArtifactQuarantineReport {
    facts: Vec<ExecutionFact>,
    preservation: RegisteredArtifactQuarantinePreservation,
}

#[must_use = "quarantine outcomes must settle the exact concurrent state"]
pub(crate) enum RegisteredArtifactQuarantineOutcome {
    Quarantined(RegisteredArtifactQuarantineReport),
    AlreadyExact(RegisteredArtifactObservedExactProof),
}

#[must_use = "parked registered-artifact authority must be acknowledged or retained"]
pub(crate) struct RegisteredArtifactQuarantinePreservation {
    parked: ParkedFile,
    root_session: Arc<AppRootSession>,
}

#[must_use = "mutation errors may retain unsettled filesystem authority"]
pub(crate) struct RegisteredArtifactMutationError {
    facts: Vec<ExecutionFact>,
    source: io::Error,
    secondary_source: Option<io::Error>,
    preservation: Option<RegisteredArtifactEffectPreservationError>,
}

#[must_use = "unsettled registered-artifact effects retain filesystem authority"]
pub(crate) enum RegisteredArtifactEffectPreservationError {
    Create {
        obligation: FileCreateObligation,
        _root_session: Arc<AppRootSession>,
    },
    Park {
        obligation: FileParkObligation,
        _root_session: Arc<AppRootSession>,
    },
    ParkAcknowledgement {
        error: FileParkPreservationError,
        _root_session: Arc<AppRootSession>,
    },
    AcknowledgementUnresolved {
        error: io::Error,
        _root_session: Arc<AppRootSession>,
    },
    Published {
        error: io::Error,
        current: FileCapability,
        _root_session: Arc<AppRootSession>,
    },
    PublishedUnresolved {
        error: io::Error,
        _root_session: Arc<AppRootSession>,
    },
    Promotion {
        obligation: FilePromotionObligation,
        _root_session: Arc<AppRootSession>,
    },
    Discard {
        obligation: StageDiscardObligation,
        _root_session: Arc<AppRootSession>,
    },
}

#[must_use = "physical observations may retain an exact-file proof"]
pub(crate) enum RegisteredArtifactPhysicalState {
    Missing,
    Exact(RegisteredArtifactObservedExactProof),
    Corrupt,
}

enum ArtifactWriteFrame {
    Bytes(Vec<u8>),
    Finish,
}

enum ArtifactQuarantineWorkerOutcome {
    Quarantined(RegisteredArtifactQuarantinePreservation),
    AlreadyExact(RegisteredArtifactObservedExactProof),
}

struct ArtifactWorkerFailure {
    kind: ExecutionFactKind,
    source: io::Error,
    preservation: Option<RegisteredArtifactEffectPreservationError>,
}

struct ArtifactStreamFailure {
    kind: ExecutionFactKind,
    source: io::Error,
    receiver_closed: bool,
}

enum RegisteredArtifactVerification {
    Exact(FileCapability, FileRevision),
    Mismatch {
        file: FileCapability,
        error: io::Error,
    },
    Uncertain {
        file: FileCapability,
        error: io::Error,
    },
}

impl RegisteredArtifactMutationCapability {
    pub(crate) async fn mint(
        root_session: Arc<AppRootSession>,
        path: KnownGoodPhysicalPath,
    ) -> io::Result<Self> {
        RegisteredArtifactLocation::mint(root_session, path)
            .await
            .map(|location| Self { location })
    }

    pub(crate) async fn quarantine_existing(
        &self,
        operation_id: &OperationId,
        target: &TargetDescriptor,
        expected_sha1: &str,
        expected_size: u64,
    ) -> Result<RegisteredArtifactQuarantineOutcome, RegisteredArtifactMutationError> {
        if expected_size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(fact_error(
                ExecutionFactKind::DownloadSizeMismatch,
                operation_id,
                target,
            ));
        }
        let location = self.location.clone();
        let expected_sha1 = expected_sha1.to_string();
        let quarantine_name = LeafName::new(format!(".axial-quarantine-{operation_id}"))
            .map_err(|_| {
                mutation_error(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "operation id does not produce a valid quarantine leaf",
                    ),
                    operation_id,
                    target,
                )
            })?;
        let result = tokio::task::spawn_blocking(move || {
            quarantine_registered_artifact(
                location,
                quarantine_name,
                &expected_sha1,
                expected_size,
            )
        })
        .await
        .map_err(|error| mutation_error(io::Error::other(error), operation_id, target))?;
        match result {
            Ok(ArtifactQuarantineWorkerOutcome::Quarantined(preservation)) => {
                Ok(RegisteredArtifactQuarantineOutcome::Quarantined(
                    RegisteredArtifactQuarantineReport {
                        facts: vec![file_fact(
                            ExecutionFactKind::FileQuarantined,
                            Some(operation_id.clone()),
                            target,
                        )],
                        preservation,
                    },
                ))
            }
            Ok(ArtifactQuarantineWorkerOutcome::AlreadyExact(proof)) => {
                Ok(RegisteredArtifactQuarantineOutcome::AlreadyExact(proof))
            }
            Err(failure) => Err(RegisteredArtifactMutationError {
                facts: vec![file_fact(
                    failure.kind,
                    Some(operation_id.clone()),
                    target,
                )],
                source: failure.source,
                secondary_source: None,
                preservation: failure.preservation,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_verify_promote(
        &self,
        operation_id: &OperationId,
        target: &TargetDescriptor,
        provider_url: &str,
        expected_sha1: &str,
        expected_size: u64,
        client: &Client,
    ) -> Result<RegisteredArtifactMutationReport, RegisteredArtifactMutationError> {
        if expected_size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(fact_error(
                ExecutionFactKind::DownloadSizeMismatch,
                operation_id,
                target,
            ));
        }
        self.ensure_target_missing()
            .await
            .map_err(|error| mutation_error(error, operation_id, target))?;
        let response = client
            .get(provider_url)
            .send()
            .await
            .map_err(|error| {
                fact_error_with_source(
                    ExecutionFactKind::DownloadNetworkFailure,
                    io::Error::other(error),
                    operation_id,
                    target,
                )
            })?;
        if !response.status().is_success() {
            return Err(fact_error(
                ExecutionFactKind::DownloadProviderFailure,
                operation_id,
                target,
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length != expected_size)
        {
            return Err(fact_error(
                ExecutionFactKind::DownloadSizeMismatch,
                operation_id,
                target,
            ));
        }

        let (sender, receiver) =
            tokio::sync::mpsc::channel::<ArtifactWriteFrame>(DOWNLOAD_FRAME_CAPACITY);
        let location = self.location.clone();
        let worker_expected_sha1 = expected_sha1.to_string();
        let mut worker = tokio::task::spawn_blocking(move || {
            stage_and_promote_registered_artifact(
                location,
                &worker_expected_sha1,
                expected_size,
                receiver,
            )
        });
        let stream = stream_registered_artifact(
            response,
            sender,
            expected_sha1,
            expected_size,
        );
        tokio::pin!(stream);
        // Prefer an independently completed provider failure when both sides are ready;
        // a pending provider still yields immediately to an exited filesystem worker.
        let (stream_result, worker_result) = tokio::select! {
            biased;
            stream_result = stream.as_mut() => {
                let worker_result = worker.await.map_err(|error| {
                    mutation_error(io::Error::other(error), operation_id, target)
                })?;
                (stream_result, worker_result)
            },
            joined = &mut worker => {
                let worker_result = joined.map_err(|error| {
                    mutation_error(io::Error::other(error), operation_id, target)
                })?;
                match worker_result {
                    Ok(proof) => (stream.as_mut().await, Ok(proof)),
                    Err(failure) => {
                        return Err(worker_mutation_error(failure, operation_id, target));
                    }
                }
            }
        };

        if let Err(stream_failure) = stream_result {
            return Err(match worker_result {
                Ok(_) => stream_mutation_error(stream_failure, operation_id, target),
                Err(worker_failure) => selected_download_mutation_error(
                    stream_failure,
                    worker_failure,
                    operation_id,
                    target,
                ),
            });
        }
        let proof = worker_result
            .map_err(|failure| worker_mutation_error(failure, operation_id, target))?;
        Ok(RegisteredArtifactMutationReport {
            facts: vec![
                execution_fact(
                    ExecutionFactKind::DownloadWrittenToTemp,
                    operation_id,
                    target,
                ),
                execution_fact(ExecutionFactKind::DownloadPromoted, operation_id, target),
            ],
            proof,
        })
    }

    async fn ensure_target_missing(&self) -> io::Result<()> {
        let location = self.location.clone();
        tokio::task::spawn_blocking(move || {
            location.parent.identity()?;
            match location.parent.open_file(&location.leaf) {
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "registered artifact target is no longer vacant",
                )),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    location.parent.identity().map(|_| ())
                }
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(crate) async fn classify(
        &self,
        expected_sha1: &str,
        expected_size: u64,
    ) -> Option<RegisteredArtifactPhysicalState> {
        if expected_size > MAX_TIER2_ARTIFACT_BYTES {
            return None;
        }
        let location = self.location.clone();
        let expected_sha1 = expected_sha1.to_string();
        tokio::task::spawn_blocking(move || {
            classify_registered_artifact(&location, &expected_sha1, expected_size)
        })
        .await
        .ok()
        .flatten()
    }
}

impl RegisteredArtifactLocation {
    async fn mint(
        root_session: Arc<AppRootSession>,
        path: KnownGoodPhysicalPath,
    ) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let mut parent = root_session.admit_absolute_directory(path.root())?;
            let mut components = path.relative().components().peekable();
            let mut leaf = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "registered artifact path is not a confined relative path",
                    ));
                };
                let name = LeafName::new(name.to_os_string()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "registered artifact path contains an invalid native leaf",
                    )
                })?;
                if components.peek().is_some() {
                    parent = parent.open_directory(&name)?;
                } else {
                    leaf = Some(name);
                }
            }
            let leaf = leaf.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "registered artifact path has no file leaf",
                )
            })?;
            parent.identity()?;
            Ok(Self {
                parent,
                leaf,
                root_session,
            })
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl RegisteredArtifactExactVerifier {
    pub(crate) async fn mint(
        root_session: Arc<AppRootSession>,
        path: KnownGoodPhysicalPath,
        expected_sha1: String,
        expected_size: u64,
    ) -> io::Result<(Self, RegisteredArtifactExactVerification)> {
        if expected_size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "registered artifact exceeds the verification bound",
            ));
        }
        let identity = Arc::new(());
        let location = RegisteredArtifactLocation::mint(root_session, path).await?;
        Ok((
            Self {
                location,
                expected_sha1,
                expected_size,
                identity: Arc::clone(&identity),
            },
            RegisteredArtifactExactVerification { identity },
        ))
    }

    pub(crate) async fn verify(self) -> Result<RegisteredArtifactExactProof, ()> {
        tokio::task::spawn_blocking(move || {
            let (file, revision) = match verify_registered_artifact(
                &self.location,
                &self.expected_sha1,
                self.expected_size,
            ) {
                Ok(RegisteredArtifactVerification::Exact(file, revision)) => (file, revision),
                Ok(RegisteredArtifactVerification::Mismatch { .. })
                | Ok(RegisteredArtifactVerification::Uncertain { .. })
                | Err(_) => return Err(()),
            };
            Ok(RegisteredArtifactExactProof {
                file,
                revision,
                _root_session: self.location.root_session,
                identity: self.identity,
                #[cfg(test)]
                lifetime: Arc::new(()),
            })
        })
        .await
        .unwrap_or(Err(()))
    }
}

impl RegisteredArtifactExactVerification {
    pub(crate) async fn validate(
        self,
        proof: RegisteredArtifactExactProof,
    ) -> Result<RegisteredArtifactExactProof, ()> {
        if !Arc::ptr_eq(&self.identity, &proof.identity) {
            return Err(());
        }
        tokio::task::spawn_blocking(move || {
            proof
                .file
                .validate_revision(&proof.revision)
                .map(|()| proof)
                .map_err(|_| ())
        })
        .await
        .unwrap_or(Err(()))
    }
}

impl RegisteredArtifactExactProof {
    #[cfg(test)]
    pub(crate) fn lifetime_for_test(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.lifetime)
    }
}

impl RegisteredArtifactQuarantineReport {
    pub(crate) fn into_parts(
        self,
    ) -> (Vec<ExecutionFact>, RegisteredArtifactQuarantinePreservation) {
        (self.facts, self.preservation)
    }
}

impl RegisteredArtifactMutationReport {
    pub(crate) fn facts(&self) -> &[ExecutionFact] {
        &self.facts
    }

    pub(crate) async fn validate(
        self,
    ) -> Result<RegisteredArtifactMutationProof, RegisteredArtifactEffectPreservationError> {
        self.proof.validate().await
    }
}

impl RegisteredArtifactMutationProof {
    pub(crate) async fn validate(
        self,
    ) -> Result<Self, RegisteredArtifactEffectPreservationError> {
        let retained_root_session = Arc::clone(&self.root_session);
        tokio::task::spawn_blocking(move || {
            match self.file.validate_revision(&self.revision) {
                Ok(()) => Ok(self),
                Err(error) => Err(RegisteredArtifactEffectPreservationError::Published {
                    error,
                    current: self.file,
                    _root_session: self.root_session,
                }),
            }
        })
        .await
        .unwrap_or_else(|error| {
            Err(RegisteredArtifactEffectPreservationError::PublishedUnresolved {
                error: io::Error::other(error),
                _root_session: retained_root_session,
            })
        })
    }
}

impl RegisteredArtifactObservedExactProof {
    pub(crate) async fn validate(
        self,
    ) -> Result<Self, RegisteredArtifactObservedExactValidationError> {
        tokio::task::spawn_blocking(move || {
            match self.file.validate_revision(&self.revision) {
                Ok(()) => Ok(self),
                Err(source) => Err(RegisteredArtifactObservedExactValidationError { source }),
            }
        })
        .await
        .unwrap_or_else(|error| {
            Err(RegisteredArtifactObservedExactValidationError {
                source: io::Error::other(error),
            })
        })
    }
}

impl std::fmt::Debug for RegisteredArtifactObservedExactValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredArtifactObservedExactValidationError")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RegisteredArtifactObservedExactValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("observed exact registered artifact is no longer current")
    }
}

impl std::error::Error for RegisteredArtifactObservedExactValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl RegisteredArtifactQuarantinePreservation {
    pub(crate) async fn acknowledge_preserved(
        self,
    ) -> Result<(), RegisteredArtifactEffectPreservationError> {
        let retained_root_session = Arc::clone(&self.root_session);
        tokio::task::spawn_blocking(move || {
            self.parked.acknowledge_preserved().map_err(|error| {
                RegisteredArtifactEffectPreservationError::ParkAcknowledgement {
                    error,
                    _root_session: self.root_session,
                }
            })
        })
        .await
        .unwrap_or_else(|error| {
            // A panicked blocking task has already marked its consumed park token abandoned;
            // retaining the root session keeps that unresolved authority available to recovery.
            Err(RegisteredArtifactEffectPreservationError::AcknowledgementUnresolved {
                error: io::Error::other(error),
                _root_session: retained_root_session,
            })
        })
    }
}

impl RegisteredArtifactMutationError {
    pub(crate) fn facts(&self) -> &[ExecutionFact] {
        &self.facts
    }

    pub(crate) fn has_unsettled_effect(&self) -> bool {
        self.preservation.is_some()
    }
}

impl std::fmt::Debug for RegisteredArtifactMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredArtifactMutationError")
            .field("has_secondary_source", &self.secondary_source.is_some())
            .field("has_preservation", &self.preservation.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RegisteredArtifactMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("registered artifact mutation failed")
    }
}

impl std::error::Error for RegisteredArtifactMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl std::fmt::Debug for RegisteredArtifactEffectPreservationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredArtifactEffectPreservationError")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RegisteredArtifactEffectPreservationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Create { .. } => formatter.write_str("registered artifact stage creation remains unsettled"),
            Self::Park { .. } => formatter.write_str("registered artifact quarantine remains unsettled"),
            Self::ParkAcknowledgement { .. } => formatter.write_str("registered artifact quarantine acknowledgement failed"),
            Self::AcknowledgementUnresolved { .. } => formatter.write_str("registered artifact quarantine acknowledgement remains unresolved"),
            Self::Published { .. } => formatter.write_str("published registered artifact requires reconciliation"),
            Self::PublishedUnresolved { .. } => formatter.write_str("published registered artifact validation remains unresolved"),
            Self::Promotion { .. } => formatter.write_str("registered artifact promotion remains unsettled"),
            Self::Discard { .. } => formatter.write_str("registered artifact stage discard remains unsettled"),
        }
    }
}

impl std::error::Error for RegisteredArtifactEffectPreservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Create { obligation, .. } => Some(obligation.error()),
            Self::Park { obligation, .. } => Some(obligation.error()),
            Self::ParkAcknowledgement { error, .. } => Some(error.error()),
            Self::AcknowledgementUnresolved { error, .. } => Some(error),
            Self::Published { error, .. } => Some(error),
            Self::PublishedUnresolved { error, .. } => Some(error),
            Self::Promotion { obligation, .. } => Some(obligation.error()),
            Self::Discard { obligation, .. } => Some(obligation.error()),
        }
    }
}

fn classify_registered_artifact(
    location: &RegisteredArtifactLocation,
    expected_sha1: &str,
    expected_size: u64,
) -> Option<RegisteredArtifactPhysicalState> {
    location.parent.identity().ok()?;
    match location.parent.open_file(&location.leaf) {
        Ok(file) => match verify_open_registered_artifact(file, expected_sha1, expected_size) {
            RegisteredArtifactVerification::Exact(file, revision) => {
                Some(RegisteredArtifactPhysicalState::Exact(
                    RegisteredArtifactObservedExactProof {
                        file,
                        revision,
                        _root_session: Arc::clone(&location.root_session),
                    },
                ))
            }
            RegisteredArtifactVerification::Mismatch { .. } => {
                Some(RegisteredArtifactPhysicalState::Corrupt)
            }
            RegisteredArtifactVerification::Uncertain { .. } => None,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => location
            .parent
            .identity()
            .ok()
            .map(|_| RegisteredArtifactPhysicalState::Missing),
        Err(_) => None,
    }
}

fn verify_registered_artifact(
    location: &RegisteredArtifactLocation,
    expected_sha1: &str,
    expected_size: u64,
) -> io::Result<RegisteredArtifactVerification> {
    location.parent.identity()?;
    let file = location.parent.open_file(&location.leaf)?;
    Ok(verify_open_registered_artifact(
        file,
        expected_sha1,
        expected_size,
    ))
}

fn verify_open_registered_artifact(
    file: FileCapability,
    expected_sha1: &str,
    expected_size: u64,
) -> RegisteredArtifactVerification {
    let revision = match file.revision() {
        Ok(revision) => revision,
        Err(error) => {
            return RegisteredArtifactVerification::Uncertain {
                file,
                error,
            };
        }
    };
    if revision.size() != expected_size {
        return match file.validate_revision(&revision) {
            Ok(()) => RegisteredArtifactVerification::Mismatch {
                file,
                error: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registered artifact size does not match its exact inventory entry",
                ),
            },
            Err(error) => RegisteredArtifactVerification::Uncertain {
                file,
                error,
            },
        };
    }
    let digest = {
        let mut reader = match file.reader(expected_size) {
            Ok(reader) => reader,
            Err(error) => {
                return RegisteredArtifactVerification::Uncertain {
                    file,
                    error,
                };
            }
        };
        let mut hasher = Sha1::new();
        let mut buffer = [0_u8; DOWNLOAD_FRAME_BYTES];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    drop(reader);
                    return RegisteredArtifactVerification::Uncertain {
                        file,
                        error,
                    };
                }
            };
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        if let Err(error) = reader.finish() {
            return RegisteredArtifactVerification::Uncertain {
                file,
                error,
            };
        }
        format!("{:x}", hasher.finalize())
    };
    if let Err(error) = file.validate_revision(&revision) {
        return RegisteredArtifactVerification::Uncertain {
            file,
            error,
        };
    }
    if digest == expected_sha1 {
        RegisteredArtifactVerification::Exact(file, revision)
    } else {
        RegisteredArtifactVerification::Mismatch {
            file,
            error: io::Error::new(
                io::ErrorKind::InvalidData,
                "registered artifact checksum does not match its exact inventory entry",
            ),
        }
    }
}

fn quarantine_registered_artifact(
    location: RegisteredArtifactLocation,
    quarantine_name: LeafName,
    expected_sha1: &str,
    expected_size: u64,
) -> Result<ArtifactQuarantineWorkerOutcome, ArtifactWorkerFailure> {
    let file = location
        .parent
        .open_file(&location.leaf)
        .map_err(|error| refused_worker_failure(error, ExecutionFactKind::PrimitiveRefused))?;
    let revision = file
        .revision()
        .map_err(|error| refused_worker_failure(error, ExecutionFactKind::PrimitiveRefused))?;
    if revision.size() > MAX_TIER2_ARTIFACT_BYTES {
        return Err(refused_worker_failure(
            io::Error::new(
                io::ErrorKind::InvalidData,
                "registered artifact exceeds the quarantine verification bound",
            ),
            ExecutionFactKind::PrimitiveRefused,
        ));
    }
    let mut reader = file
        .reader(revision.size())
        .map_err(|error| refused_worker_failure(error, ExecutionFactKind::PrimitiveRefused))?;
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut buffer = [0_u8; DOWNLOAD_FRAME_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| refused_worker_failure(error, ExecutionFactKind::PrimitiveRefused))?;
        if read == 0 {
            break;
        }
        sha1.update(&buffer[..read]);
        sha256.update(&buffer[..read]);
    }
    reader
        .finish()
        .map_err(|error| refused_worker_failure(error, ExecutionFactKind::PrimitiveRefused))?;
    file.validate_revision(&revision)
        .map_err(|error| refused_worker_failure(error, ExecutionFactKind::PrimitiveRefused))?;
    if revision.size() == expected_size && format!("{:x}", sha1.finalize()) == expected_sha1 {
        return Ok(ArtifactQuarantineWorkerOutcome::AlreadyExact(
            RegisteredArtifactObservedExactProof {
                file,
                revision,
                _root_session: location.root_session,
            },
        ));
    }
    let sha256: [u8; 32] = sha256.finalize().into();
    let request = file.park_request(ExpectedFileContent::new(revision, sha256));
    settle_artifact_park(
        location.parent.park_file_as(request, quarantine_name),
        location.root_session,
    )
    .map(ArtifactQuarantineWorkerOutcome::Quarantined)
}

fn settle_artifact_park(
    outcome: FileParkOutcome,
    root_session: Arc<AppRootSession>,
) -> Result<RegisteredArtifactQuarantinePreservation, ArtifactWorkerFailure> {
    match outcome {
        FileParkOutcome::Parked(parked) => Ok(RegisteredArtifactQuarantinePreservation {
            parked,
            root_session,
        }),
        FileParkOutcome::NoEffect { error, .. } => Err(refused_worker_failure(
            error,
            ExecutionFactKind::PrimitiveRefused,
        )),
        FileParkOutcome::AppliedUnverified(obligation) => {
            let kind = io_fact_kind(obligation.error());
            let source = io::Error::new(
                obligation.error().kind(),
                obligation.error().to_string(),
            );
            match obligation.reconcile() {
                FileParkResolution::Parked(parked) => {
                    Ok(RegisteredArtifactQuarantinePreservation {
                        parked,
                        root_session,
                    })
                }
                FileParkResolution::NoEffect(_) => Err(ArtifactWorkerFailure {
                    kind,
                    source,
                    preservation: None,
                }),
                FileParkResolution::Indeterminate(obligation) => Err(ArtifactWorkerFailure {
                    kind,
                    source,
                    preservation: Some(RegisteredArtifactEffectPreservationError::Park {
                        obligation,
                        _root_session: root_session,
                    }),
                }),
            }
        }
    }
}

async fn stream_registered_artifact(
    response: reqwest::Response,
    sender: tokio::sync::mpsc::Sender<ArtifactWriteFrame>,
    expected_sha1: &str,
    expected_size: u64,
) -> Result<(), ArtifactStreamFailure> {
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut observed = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ArtifactStreamFailure {
            kind: ExecutionFactKind::DownloadInterrupted,
            source: io::Error::other(error),
            receiver_closed: false,
        })?;
        observed = observed
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| ArtifactStreamFailure {
                kind: ExecutionFactKind::DownloadSizeMismatch,
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registered artifact provider size overflowed",
                ),
                receiver_closed: false,
            })?;
        if observed > expected_size || observed > MAX_TIER2_ARTIFACT_BYTES {
            return Err(ArtifactStreamFailure {
                kind: ExecutionFactKind::DownloadSizeMismatch,
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registered artifact provider exceeded the expected size",
                ),
                receiver_closed: false,
            });
        }
        hasher.update(&chunk);
        for frame in chunk.chunks(DOWNLOAD_FRAME_BYTES) {
            sender
                .send(ArtifactWriteFrame::Bytes(frame.to_vec()))
                .await
                .map_err(|_| ArtifactStreamFailure {
                    kind: ExecutionFactKind::DownloadTempWriteFailed,
                    source: io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "registered artifact stage writer stopped",
                    ),
                    receiver_closed: true,
                })?;
        }
    }
    if observed != expected_size {
        return Err(ArtifactStreamFailure {
            kind: ExecutionFactKind::DownloadSizeMismatch,
            source: io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "registered artifact provider ended at the wrong size",
            ),
            receiver_closed: false,
        });
    }
    if format!("{:x}", hasher.finalize()) != expected_sha1 {
        return Err(ArtifactStreamFailure {
            kind: ExecutionFactKind::DownloadChecksumMismatch,
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "registered artifact provider checksum did not match",
            ),
            receiver_closed: false,
        });
    }
    sender
        .send(ArtifactWriteFrame::Finish)
        .await
        .map_err(|_| ArtifactStreamFailure {
            kind: ExecutionFactKind::DownloadTempWriteFailed,
            source: io::Error::new(
                io::ErrorKind::BrokenPipe,
                "registered artifact stage writer stopped before settlement",
            ),
            receiver_closed: true,
        })
}

fn stage_and_promote_registered_artifact(
    location: RegisteredArtifactLocation,
    expected_sha1: &str,
    expected_size: u64,
    mut receiver: tokio::sync::mpsc::Receiver<ArtifactWriteFrame>,
) -> Result<RegisteredArtifactMutationProof, ArtifactWorkerFailure> {
    let mut staged = settle_stage_create(location.parent.create_stage(), &location.root_session)?;
    let write_result = write_staged_artifact(&mut staged, expected_size, &mut receiver);
    if let Err(error) = write_result {
        return Err(discard_staged_after_failure(
            staged,
            error,
            ExecutionFactKind::DownloadTempWriteFailed,
            &location.root_session,
        ));
    }
    let sealed = match staged.seal() {
        Ok(sealed) => sealed,
        Err(failure) => {
            let error = io::Error::new(failure.error().kind(), failure.error().to_string());
            return Err(discard_staged_after_failure(
                failure.into_staged(),
                error,
                ExecutionFactKind::DownloadTempWriteFailed,
                &location.root_session,
            ));
        }
    };
    let promotion = sealed.promote_no_replace(
        &location.parent,
        &location.parent,
        &location.leaf,
    );
    settle_artifact_promotion(
        promotion,
        expected_sha1,
        expected_size,
        location.root_session,
    )
}

fn settle_stage_create(
    outcome: FileCreateOutcome,
    root_session: &Arc<AppRootSession>,
) -> Result<StagedFile, ArtifactWorkerFailure> {
    match outcome {
        FileCreateOutcome::Created(staged) => Ok(staged),
        FileCreateOutcome::NoEffect(error) => Err(refused_worker_failure(
            error,
            ExecutionFactKind::DownloadTempWriteFailed,
        )),
        FileCreateOutcome::AppliedUnverified(obligation) => {
            let source = io::Error::new(
                obligation.error().kind(),
                obligation.error().to_string(),
            );
            match obligation.reconcile() {
                FileCreateResolution::Created(staged) => Ok(staged),
                FileCreateResolution::Indeterminate(obligation) => Err(ArtifactWorkerFailure {
                    kind: ExecutionFactKind::DownloadTempWriteFailed,
                    source,
                    preservation: Some(RegisteredArtifactEffectPreservationError::Create {
                        obligation,
                        _root_session: Arc::clone(root_session),
                    }),
                }),
            }
        }
    }
}

fn write_staged_artifact(
    staged: &mut StagedFile,
    expected_size: u64,
    receiver: &mut tokio::sync::mpsc::Receiver<ArtifactWriteFrame>,
) -> io::Result<()> {
    let mut writer = staged.writer()?;
    let mut observed = 0_u64;
    loop {
        match receiver.blocking_recv() {
            Some(ArtifactWriteFrame::Bytes(bytes)) => {
                if bytes.len() > DOWNLOAD_FRAME_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "registered artifact download frame exceeded its bound",
                    ));
                }
                observed = observed.checked_add(bytes.len() as u64).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "artifact size overflowed")
                })?;
                if observed > expected_size || observed > MAX_TIER2_ARTIFACT_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "registered artifact download exceeded its expected size",
                    ));
                }
                writer.write_all(&bytes)?;
            }
            Some(ArtifactWriteFrame::Finish) if observed == expected_size => {
                return writer.finish();
            }
            Some(ArtifactWriteFrame::Finish) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "registered artifact download ended at the wrong size",
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "registered artifact download was cancelled",
                ));
            }
        }
    }
}

fn settle_artifact_promotion(
    outcome: FilePromotionOutcome,
    expected_sha1: &str,
    expected_size: u64,
    root_session: Arc<AppRootSession>,
) -> Result<RegisteredArtifactMutationProof, ArtifactWorkerFailure> {
    match outcome {
        FilePromotionOutcome::Applied(current) => settle_promoted_artifact(
            current,
            expected_sha1,
            expected_size,
            root_session,
        ),
        FilePromotionOutcome::NoEffect { error, staged } => Err(
            discard_sealed_after_failure(
                staged,
                error,
                ExecutionFactKind::DownloadPromotionFailed,
                &root_session,
            ),
        ),
        FilePromotionOutcome::AppliedUnverified(obligation) => {
            let source = io::Error::new(
                obligation.error().kind(),
                obligation.error().to_string(),
            );
            match obligation.reconcile() {
                FilePromotionResolution::Applied(current) => settle_promoted_artifact(
                    current,
                    expected_sha1,
                    expected_size,
                    root_session,
                ),
                FilePromotionResolution::NoEffect(staged) => Err(
                    discard_sealed_after_failure(
                        staged,
                        source,
                        ExecutionFactKind::DownloadPromotionFailed,
                        &root_session,
                    ),
                ),
                FilePromotionResolution::Indeterminate(obligation) => {
                    Err(ArtifactWorkerFailure {
                        kind: ExecutionFactKind::DownloadPromotionFailed,
                        source,
                        preservation: Some(
                            RegisteredArtifactEffectPreservationError::Promotion {
                                obligation,
                                _root_session: root_session,
                            },
                        ),
                    })
                }
            }
        }
    }
}

fn settle_promoted_artifact(
    current: FileCapability,
    expected_sha1: &str,
    expected_size: u64,
    root_session: Arc<AppRootSession>,
) -> Result<RegisteredArtifactMutationProof, ArtifactWorkerFailure> {
    match verify_open_registered_artifact(current, expected_sha1, expected_size) {
        RegisteredArtifactVerification::Exact(file, revision) => {
            Ok(RegisteredArtifactMutationProof {
                file,
                revision,
                root_session,
            })
        }
        RegisteredArtifactVerification::Mismatch { file, error } => {
            Err(published_artifact_failure(file, error, root_session))
        }
        RegisteredArtifactVerification::Uncertain { file, error } => {
            Err(published_artifact_failure(file, error, root_session))
        }
    }
}

fn published_artifact_failure(
    current: FileCapability,
    source: io::Error,
    root_session: Arc<AppRootSession>,
) -> ArtifactWorkerFailure {
    let preservation_error = io::Error::new(source.kind(), source.to_string());
    ArtifactWorkerFailure {
        kind: ExecutionFactKind::DownloadPromotionFailed,
        source,
        preservation: Some(RegisteredArtifactEffectPreservationError::Published {
            error: preservation_error,
            current,
            _root_session: root_session,
        }),
    }
}

fn discard_staged_after_failure(
    staged: StagedFile,
    error: io::Error,
    kind: ExecutionFactKind,
    root_session: &Arc<AppRootSession>,
) -> ArtifactWorkerFailure {
    settle_stage_discard(staged.discard(), error, kind, root_session)
}

fn discard_sealed_after_failure(
    staged: SealedStagedFile,
    error: io::Error,
    kind: ExecutionFactKind,
    root_session: &Arc<AppRootSession>,
) -> ArtifactWorkerFailure {
    settle_stage_discard(staged.discard(), error, kind, root_session)
}

fn settle_stage_discard(
    outcome: StageDiscardOutcome,
    error: io::Error,
    kind: ExecutionFactKind,
    root_session: &Arc<AppRootSession>,
) -> ArtifactWorkerFailure {
    match outcome {
        StageDiscardOutcome::Discarded => ArtifactWorkerFailure {
            kind,
            source: error,
            preservation: None,
        },
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            match obligation.reconcile() {
                StageDiscardResolution::Discarded => ArtifactWorkerFailure {
                    kind,
                    source: error,
                    preservation: None,
                },
                StageDiscardResolution::Indeterminate(obligation) => ArtifactWorkerFailure {
                    kind,
                    source: error,
                    preservation: Some(RegisteredArtifactEffectPreservationError::Discard {
                        obligation,
                        _root_session: Arc::clone(root_session),
                    }),
                },
            }
        }
    }
}

fn refused_worker_failure(error: io::Error, fallback: ExecutionFactKind) -> ArtifactWorkerFailure {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => ExecutionFactKind::FileMissing,
        io::ErrorKind::PermissionDenied => ExecutionFactKind::FilePermissionDenied,
        _ => fallback,
    };
    ArtifactWorkerFailure {
        kind,
        source: error,
        preservation: None,
    }
}

fn io_fact_kind(error: &io::Error) -> ExecutionFactKind {
    match error.kind() {
        io::ErrorKind::NotFound => ExecutionFactKind::FileMissing,
        io::ErrorKind::PermissionDenied => ExecutionFactKind::FilePermissionDenied,
        _ => ExecutionFactKind::PrimitiveRefused,
    }
}

fn mutation_error(
    error: io::Error,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    fact_error_with_source(io_fact_kind(&error), error, operation_id, target)
}

fn worker_mutation_error(
    failure: ArtifactWorkerFailure,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    RegisteredArtifactMutationError {
        facts: vec![execution_fact(failure.kind, operation_id, target)],
        source: failure.source,
        secondary_source: None,
        preservation: failure.preservation,
    }
}

fn stream_mutation_error(
    failure: ArtifactStreamFailure,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    RegisteredArtifactMutationError {
        facts: vec![execution_fact(failure.kind, operation_id, target)],
        source: failure.source,
        secondary_source: None,
        preservation: None,
    }
}

fn selected_download_mutation_error(
    stream: ArtifactStreamFailure,
    worker: ArtifactWorkerFailure,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    if stream.receiver_closed {
        return worker_mutation_error(worker, operation_id, target);
    }
    RegisteredArtifactMutationError {
        facts: vec![execution_fact(stream.kind, operation_id, target)],
        source: stream.source,
        secondary_source: Some(worker.source),
        preservation: worker.preservation,
    }
}

fn fact_error(
    kind: ExecutionFactKind,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    fact_error_with_source(
        kind,
        io::Error::other(match kind {
            ExecutionFactKind::DownloadProviderFailure => {
                "registered artifact provider refused the download"
            }
            ExecutionFactKind::DownloadSizeMismatch => {
                "registered artifact download size was invalid"
            }
            ExecutionFactKind::PrimitiveRefused => {
                "registered artifact primitive was refused"
            }
            _ => "registered artifact mutation failed",
        }),
        operation_id,
        target,
    )
}

fn fact_error_with_source(
    kind: ExecutionFactKind,
    source: io::Error,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    RegisteredArtifactMutationError {
        facts: vec![execution_fact(kind, operation_id, target)],
        source,
        secondary_source: None,
        preservation: None,
    }
}

fn execution_fact(
    kind: ExecutionFactKind,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> ExecutionFact {
    file_fact(kind, Some(operation_id.clone()), target)
}

#[cfg(test)]
mod tests {
    use super::{
        ArtifactStreamFailure, ArtifactWorkerFailure,
        ExecutionFactKind,
        RegisteredArtifactEffectPreservationError, RegisteredArtifactExactVerifier,
        RegisteredArtifactMutationCapability, RegisteredArtifactObservedExactValidationError,
        RegisteredArtifactQuarantineOutcome,
        selected_download_mutation_error,
    };
    use crate::state::contracts::{
        OperationId, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use axial_config::AppPaths;
    use axial_minecraft::known_good::KnownGoodPhysicalPath;
    use sha1::Digest as _;
    use std::fs;
    use std::io::{self, Read as _, Write as _};
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;

    fn root_session(base: &std::path::Path) -> Arc<axial_config::AppRootSession> {
        fs::create_dir_all(base).expect("create test application root");
        let paths = AppPaths::from_root(base.to_path_buf()).expect("absolute test app root");
        crate::state::test_root_session(&paths)
    }

    fn target(id: &str) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            id,
            OwnershipClass::LauncherManaged,
        )
    }

    #[tokio::test]
    async fn zero_byte_download_is_verified_and_promoted() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind zero-byte artifact server");
        let address = listener.local_addr().expect("zero-byte server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept zero-byte request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write zero-byte response");
        });
        let base =
            std::env::temp_dir().join(format!("axial-zero-byte-artifact-{}", uuid::Uuid::new_v4()));
        let relative = PathBuf::from("assets/objects/da/da39a3ee5e6b4b0d3255bfef95601890afd80709");
        fs::create_dir_all(base.join(relative.parent().expect("zero-byte artifact parent")))
            .expect("create zero-byte artifact parent");
        let root_session = root_session(&base);
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint zero-byte artifact capability");
        let operation_id = OperationId::deterministic_test("zero-byte-artifact-promotion");
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "zero-byte-artifact-promotion",
            OwnershipClass::LauncherManaged,
        );

        let result = capability
            .download_verify_promote(
                &operation_id,
                &target,
                &format!("http://{address}/artifact"),
                "da39a3ee5e6b4b0d3255bfef95601890afd80709",
                0,
                &reqwest::Client::new(),
            )
            .await;
        let proof = result
            .expect("download and promote zero-byte artifact")
            .validate()
            .await
            .expect("validate promoted zero-byte proof");
        assert_eq!(
            fs::metadata(base.join(&relative))
                .expect("promoted zero-byte artifact")
                .len(),
            0
        );
        server.join().expect("join zero-byte artifact server");
        drop(proof);
        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove zero-byte artifact fixture");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ancestor_swap_cannot_redirect_quarantine_outside_the_held_root() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-confinement-{}",
            uuid::Uuid::new_v4()
        ));
        let managed_root = base.join("managed");
        let detached_root = base.join("detached-managed");
        let outside_root = base.join("outside");
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(managed_root.join("libraries/example"))
            .expect("create managed artifact parent");
        fs::create_dir_all(outside_root.join("libraries/example"))
            .expect("create outside artifact parent");
        fs::write(managed_root.join(&relative), b"managed-corrupt")
            .expect("write managed artifact");
        fs::write(outside_root.join(&relative), b"user-owned").expect("write outside artifact");
        let root_session = root_session(&base.join("app"));

        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(managed_root.clone(), relative.clone()),
        )
        .await
        .expect("mint confined mutation capability");
        fs::rename(&managed_root, &detached_root).expect("detach held managed root");
        symlink(&outside_root, &managed_root).expect("redirect configured root");

        let result = capability
            .quarantine_existing(
                &OperationId::deterministic_test("registered-artifact-confinement-test"),
                &TargetDescriptor::new(
                    StabilizationSystem::Execution,
                    TargetKind::Artifact,
                    "registered-artifact-confinement-test",
                    OwnershipClass::LauncherManaged,
                ),
                "deadbeef",
                1,
            )
            .await;

        assert!(result.is_err(), "ancestor drift must fail closed");
        assert_eq!(
            fs::read(outside_root.join(&relative)).expect("read outside artifact"),
            b"user-owned"
        );
        assert_eq!(
            fs::read(detached_root.join(&relative)).expect("read held managed artifact"),
            b"managed-corrupt"
        );
        assert!(
            fs::read_dir(outside_root.join("libraries/example"))
                .expect("read outside artifact parent")
                .all(|entry| !entry
                    .expect("outside directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-quarantine-"))
        );

        drop(capability);
        drop(root_session);
        fs::remove_file(&managed_root).expect("remove redirected root link");
        fs::remove_dir_all(&base).expect("remove confinement fixture");
    }

    #[tokio::test]
    async fn hard_link_alias_prevents_registered_artifact_quarantine() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-hard-link-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        let source = base.join(&relative);
        let alias = base.join("libraries/example/alias.jar");
        fs::create_dir_all(source.parent().expect("managed artifact parent"))
            .expect("create managed artifact parent");
        fs::write(&source, b"managed-corrupt").expect("write managed artifact");
        fs::hard_link(&source, &alias).expect("create managed artifact alias");
        let root_session = root_session(&base.join("app"));

        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative),
        )
        .await
        .expect("mint confined mutation capability");
        let result = capability
            .quarantine_existing(
                &OperationId::deterministic_test("registered-artifact-hard-link-test"),
                &TargetDescriptor::new(
                    StabilizationSystem::Execution,
                    TargetKind::Artifact,
                    "registered-artifact-hard-link-test",
                    OwnershipClass::LauncherManaged,
                ),
                "deadbeef",
                1,
            )
            .await;

        assert!(result.is_err(), "hard-linked artifact must fail closed");
        assert_eq!(fs::read(&source).expect("read source"), b"managed-corrupt");
        assert_eq!(fs::read(&alias).expect("read alias"), b"managed-corrupt");
        assert!(
            fs::read_dir(source.parent().expect("managed artifact parent"))
                .expect("read managed artifact parent")
                .all(|entry| !entry
                    .expect("managed artifact entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-quarantine-"))
        );

        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove hard-link fixture");
    }

    #[tokio::test]
    async fn quarantine_receipt_is_acknowledged_only_after_the_caller_accepts_it() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-quarantine-ack-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        fs::write(base.join(&relative), b"corrupt").expect("write corrupt artifact");
        let root_session = root_session(&base.join("app"));
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint artifact capability");
        let operation_id = OperationId::deterministic_test("quarantine-acknowledgement");
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "quarantine-acknowledgement",
            OwnershipClass::LauncherManaged,
        );

        let outcome = capability
            .quarantine_existing(&operation_id, &target, "deadbeef", 8)
            .await
            .expect("quarantine corrupt artifact");
        let RegisteredArtifactQuarantineOutcome::Quarantined(report) = outcome else {
            panic!("corrupt artifact was concurrently exact");
        };
        let (facts, preservation) = report.into_parts();
        assert_eq!(facts.len(), 1);
        let parked = base
            .join("libraries/example")
            .join(format!(".axial-quarantine-{operation_id}"));
        assert!(!base.join(&relative).exists());
        assert_eq!(fs::read(&parked).expect("read parked artifact"), b"corrupt");
        preservation
            .acknowledge_preserved()
            .await
            .expect("acknowledge durable quarantine evidence");
        assert_eq!(fs::read(&parked).expect("read preserved artifact"), b"corrupt");

        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove quarantine acknowledgement fixture");
    }

    #[tokio::test]
    async fn failed_quarantine_acknowledgement_retains_the_parked_file() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-quarantine-ack-failure-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        fs::write(base.join(&relative), b"corrupt").expect("write corrupt artifact");
        let root_session = root_session(&base.join("app"));
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint artifact capability");
        let operation_id = OperationId::deterministic_test("quarantine-acknowledgement-failure");
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "quarantine-acknowledgement-failure",
            OwnershipClass::LauncherManaged,
        );
        let outcome = capability
            .quarantine_existing(&operation_id, &target, "deadbeef", 8)
            .await
            .expect("quarantine corrupt artifact");
        let RegisteredArtifactQuarantineOutcome::Quarantined(report) = outcome else {
            panic!("corrupt artifact was concurrently exact");
        };
        let (_, preservation) = report.into_parts();
        let parked = base
            .join("libraries/example")
            .join(format!(".axial-quarantine-{operation_id}"));
        let displaced = base.join("libraries/example/displaced.jar");
        fs::rename(&parked, &displaced).expect("displace parked artifact");

        let error = preservation
            .acknowledge_preserved()
            .await
            .expect_err("changed park binding must retain acknowledgement authority");
        let (parked_file, retained_root_session) = match error {
            RegisteredArtifactEffectPreservationError::ParkAcknowledgement {
                error,
                _root_session,
            } => (error.into_parked(), _root_session),
            _ => panic!("unexpected quarantine preservation failure"),
        };
        fs::rename(&displaced, &parked).expect("restore parked binding");
        parked_file
            .acknowledge_preserved()
            .expect("settle restored parked artifact");

        drop(capability);
        drop(retained_root_session);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove failed acknowledgement fixture");
    }

    #[tokio::test]
    async fn target_appearing_during_download_is_not_replaced_and_stage_is_discarded() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-no-replace-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        let destination = base.join(&relative);
        let server_destination = destination.clone();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind artifact server");
        let address = listener.local_addr().expect("artifact server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept artifact request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            fs::write(&server_destination, b"concurrent").expect("publish concurrent target");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrect")
                .expect("write artifact response");
        });
        let root_session = root_session(&base.join("app"));
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint artifact capability");
        let operation_id = OperationId::deterministic_test("artifact-no-replace");
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "artifact-no-replace",
            OwnershipClass::LauncherManaged,
        );
        let expected_sha1 = format!("{:x}", sha1::Sha1::digest(b"correct"));

        let result = capability
            .download_verify_promote(
                &operation_id,
                &target,
                &format!("http://{address}/artifact"),
                &expected_sha1,
                7,
                &reqwest::Client::new(),
            )
            .await;
        assert!(result.is_err(), "concurrent target must refuse promotion");
        assert_eq!(fs::read(&destination).expect("read concurrent target"), b"concurrent");
        assert!(
            fs::read_dir(destination.parent().expect("artifact parent"))
                .expect("read artifact parent")
                .all(|entry| !entry
                    .expect("artifact directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-stage-"))
        );
        server.join().expect("join artifact server");

        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove no-replace fixture");
    }

    #[tokio::test]
    async fn changed_published_leaf_returns_the_exact_proof_carrier() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-published-proof-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind artifact server");
        let address = listener.local_addr().expect("artifact server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept artifact request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrect")
                .expect("write artifact response");
        });
        let root_session = root_session(&base.join("app"));
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint artifact capability");
        let expected_sha1 = format!("{:x}", sha1::Sha1::digest(b"correct"));
        let report = capability
            .download_verify_promote(
                &OperationId::deterministic_test("changed-published-proof"),
                &target("changed-published-proof"),
                &format!("http://{address}/artifact"),
                &expected_sha1,
                7,
                &reqwest::Client::new(),
            )
            .await
            .expect("download exact artifact");
        assert_eq!(report.facts().len(), 2);
        fs::write(base.join(&relative), b"changed-long").expect("change published artifact");

        let result = report.validate().await;
        let Err(error) = result else {
            panic!("changed published artifact must retain its proof carrier");
        };
        let RegisteredArtifactEffectPreservationError::Published {
            current,
            _root_session,
            ..
        } = error
        else {
            panic!("changed published artifact returned the wrong carrier");
        };
        assert!(Arc::ptr_eq(&_root_session, &root_session));
        assert_eq!(
            current
                .revision()
                .expect("retained current capability")
                .size(),
            12
        );
        server.join().expect("join artifact server");

        drop(current);
        drop(_root_session);
        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove published proof fixture");
    }

    #[tokio::test]
    async fn already_exact_quarantine_outcome_leaves_the_target_in_place() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-already-exact-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        fs::write(base.join(&relative), b"exact").expect("write exact artifact");
        let root_session = root_session(&base.join("app"));
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint artifact capability");
        let operation_id = OperationId::deterministic_test("already-exact-quarantine");
        let expected_sha1 = format!("{:x}", sha1::Sha1::digest(b"exact"));

        let outcome = capability
            .quarantine_existing(
                &operation_id,
                &target("already-exact-quarantine"),
                &expected_sha1,
                5,
            )
            .await
            .expect("classify exact quarantine race");
        let RegisteredArtifactQuarantineOutcome::AlreadyExact(proof) = outcome else {
            panic!("exact artifact must return an exact proof");
        };
        assert_eq!(fs::read(base.join(&relative)).expect("read exact target"), b"exact");
        assert!(!base
            .join("libraries/example")
            .join(format!(".axial-quarantine-{operation_id}"))
            .exists());

        fs::write(base.join(&relative), b"changed").expect("change observed exact target");
        let error: RegisteredArtifactObservedExactValidationError =
            match proof.validate().await {
                Err(error) => error,
                Ok(_) => {
                    panic!("changed observed artifact must invalidate its observation proof")
                }
            };
        assert!(std::error::Error::source(&error).is_some());

        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove exact quarantine fixture");
    }

    #[tokio::test]
    async fn existing_target_is_refused_before_any_http_request() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-preflight-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        fs::write(base.join(&relative), b"existing").expect("write existing artifact");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind unused provider");
        listener
            .set_nonblocking(true)
            .expect("make unused provider nonblocking");
        let address = listener.local_addr().expect("unused provider address");
        let root_session = root_session(&base.join("app"));
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative),
        )
        .await
        .expect("mint artifact capability");

        let result = capability
            .download_verify_promote(
                &OperationId::deterministic_test("existing-target-preflight"),
                &target("existing-target-preflight"),
                &format!("http://{address}/artifact"),
                "deadbeef",
                8,
                &reqwest::Client::new(),
            )
            .await;
        let Err(error) = result else {
            panic!("existing target must fail before provider access");
        };
        assert_eq!(error.facts()[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert_eq!(error.source.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            listener.accept().expect_err("provider must receive no request").kind(),
            io::ErrorKind::WouldBlock
        );

        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove existing target fixture");
    }

    #[test]
    fn receiver_closed_download_failure_selects_the_worker_cause() {
        let operation_id = OperationId::deterministic_test("receiver-closed-selection");
        let target = target("receiver-closed-selection");
        let error = selected_download_mutation_error(
            ArtifactStreamFailure {
                kind: ExecutionFactKind::DownloadTempWriteFailed,
                source: io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"),
                receiver_closed: true,
            },
            ArtifactWorkerFailure {
                kind: ExecutionFactKind::FilePermissionDenied,
                source: io::Error::new(io::ErrorKind::PermissionDenied, "stage refused"),
                preservation: None,
            },
            &operation_id,
            &target,
        );

        assert_eq!(error.facts()[0].kind, ExecutionFactKind::FilePermissionDenied);
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.secondary_source.is_none());

        let error = selected_download_mutation_error(
            ArtifactStreamFailure {
                kind: ExecutionFactKind::DownloadChecksumMismatch,
                source: io::Error::new(io::ErrorKind::InvalidData, "provider mismatch"),
                receiver_closed: false,
            },
            ArtifactWorkerFailure {
                kind: ExecutionFactKind::DownloadTempWriteFailed,
                source: io::Error::new(io::ErrorKind::Interrupted, "stage cancelled"),
                preservation: None,
            },
            &operation_id,
            &target,
        );
        assert_eq!(
            error.facts()[0].kind,
            ExecutionFactKind::DownloadChecksumMismatch
        );
        assert_eq!(error.source.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error
                .secondary_source
                .as_ref()
                .expect("worker source remains retained")
                .kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[tokio::test]
    async fn checksum_and_size_failures_discard_the_bounded_stage() {
        let correct_sha1 = format!("{:x}", sha1::Sha1::digest(b"correct"));
        assert_failed_download_cleanup(
            "checksum-cleanup",
            b"wronggg",
            &correct_sha1,
            7,
            ExecutionFactKind::DownloadChecksumMismatch,
        )
        .await;
        assert_failed_download_cleanup(
            "short-cleanup",
            b"short",
            &correct_sha1,
            7,
            ExecutionFactKind::DownloadSizeMismatch,
        )
        .await;
        assert_failed_download_cleanup(
            "oversize-cleanup",
            b"oversize",
            &correct_sha1,
            7,
            ExecutionFactKind::DownloadSizeMismatch,
        )
        .await;
    }

    async fn assert_failed_download_cleanup(
        label: &str,
        body: &'static [u8],
        expected_sha1: &str,
        expected_size: u64,
        expected_kind: ExecutionFactKind,
    ) {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind artifact server");
        let address = listener.local_addr().expect("artifact server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept artifact request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .expect("write artifact response header");
            stream.write_all(body).expect("write artifact response body");
        });
        let root_session = root_session(&base.join("app"));
        let capability = RegisteredArtifactMutationCapability::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint artifact capability");
        let result = capability
            .download_verify_promote(
                &OperationId::deterministic_test(label),
                &target(label),
                &format!("http://{address}/artifact"),
                expected_sha1,
                expected_size,
                &reqwest::Client::new(),
            )
            .await;
        let Err(error) = result else {
            panic!("invalid provider body must fail");
        };

        assert_eq!(error.facts()[0].kind, expected_kind);
        assert!(error.preservation.is_none());
        assert!(!base.join(&relative).exists());
        assert!(
            fs::read_dir(base.join("libraries/example"))
                .expect("read artifact parent")
                .all(|entry| !entry
                    .expect("artifact directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-stage-"))
        );
        server.join().expect("join artifact server");
        drop(capability);
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove failed download fixture");
    }

    #[tokio::test]
    async fn exact_verification_rejects_a_foreign_proof() {
        let (base, relative, root_session) = exact_proof_fixture("foreign-proof");
        let path = KnownGoodPhysicalPath::for_test(base.clone(), relative.clone());
        let (verifier, _) = RegisteredArtifactExactVerifier::mint(
            Arc::clone(&root_session),
            path,
            format!("{:x}", sha1::Sha1::digest(b"exact")),
            5,
        )
        .await
        .expect("mint exact verifier");
        let (_, foreign_verification) = RegisteredArtifactExactVerifier::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative),
            format!("{:x}", sha1::Sha1::digest(b"exact")),
            5,
        )
        .await
        .expect("mint foreign verification");
        let proof = verifier.verify().await.expect("verify exact artifact");

        assert!(foreign_verification.validate(proof).await.is_err());
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove foreign proof fixture");
    }

    #[tokio::test]
    async fn exact_verification_rejects_a_changed_leaf() {
        let (base, relative, root_session) = exact_proof_fixture("changed-proof");
        let (verifier, verification) = RegisteredArtifactExactVerifier::mint(
            Arc::clone(&root_session),
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
            format!("{:x}", sha1::Sha1::digest(b"exact")),
            5,
        )
        .await
        .expect("mint exact verifier");
        let proof = verifier.verify().await.expect("verify exact artifact");
        fs::write(base.join(relative), b"changed").expect("change verified artifact");

        assert!(verification.validate(proof).await.is_err());
        drop(root_session);
        fs::remove_dir_all(&base).expect("remove changed proof fixture");
    }

    fn exact_proof_fixture(
        label: &str,
    ) -> (PathBuf, PathBuf, Arc<axial_config::AppRootSession>) {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(base.join("libraries/example")).expect("create artifact parent");
        fs::write(base.join(&relative), b"exact").expect("write exact artifact");
        let root_session = root_session(&base.join("app"));
        (base, relative, root_session)
    }
}
