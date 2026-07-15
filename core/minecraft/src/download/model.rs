use crate::artifact_path::ArtifactRelativePath;
use serde::{Deserialize, Serialize};
use std::io;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub phase: String,
    pub current: i32,
    pub total: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub done: bool,
    /// Cumulative transfer-plan facts for the whole install: bytes of planned
    /// work completed vs. planned so far. Stamped by the installer entry
    /// points; absent on events emitted before the plan has any entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("file operation failed: {0}")]
    FileOperation(#[from] io::Error),
    #[error("resolve manifest url: {0}")]
    ResolveManifest(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("parse version json: {0}")]
    ParseVersion(#[from] serde_json::Error),
    #[error("prepare java runtime: {0}")]
    PrepareRuntime(String),
    #[error("java runtime {component} is not available for {platform}")]
    RuntimeUnavailableForPlatform { component: String, platform: String },
    #[error(
        "java runtime {component} needs Rosetta 2 on this Mac: run `softwareupdate --install-rosetta --agree-to-license` in Terminal"
    )]
    RuntimeRosettaRequired { component: String },
    #[error("download integrity: {0}")]
    Integrity(String),
    #[error(transparent)]
    LibraryPlan(#[from] LibraryPlanError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LibraryPlanError {
    #[error("library metadata contains an unsafe artifact path")]
    InvalidArtifactPath,
    #[error("library metadata contains an invalid checksum")]
    InvalidChecksum,
    #[error("library artifact has no download source")]
    MissingDownloadSource,
    #[error("library artifacts have conflicting contracts for the same path")]
    ConflictingArtifactPath,
    #[error("library artifact integrity metadata conflicts across representations")]
    ConflictingArtifactIntegrity,
}

pub(crate) struct ExactLibraryDownloadProof {
    path: ArtifactRelativePath,
    is_native: bool,
    provider_url: String,
    expected: ExpectedIntegrity,
    size: u64,
    sha1: [u8; 20],
}

impl ExactLibraryDownloadProof {
    pub(super) fn new(
        path: ArtifactRelativePath,
        is_native: bool,
        provider_url: String,
        expected: ExpectedIntegrity,
        size: u64,
        sha1: [u8; 20],
    ) -> Self {
        Self {
            path,
            is_native,
            provider_url,
            expected,
            size,
            sha1,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ArtifactRelativePath,
        bool,
        String,
        ExpectedIntegrity,
        u64,
        [u8; 20],
    ) {
        (
            self.path,
            self.is_native,
            self.provider_url,
            self.expected,
            self.size,
            self.sha1,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_bound_for_test(
        path: ArtifactRelativePath,
        is_native: bool,
        provider_url: String,
        expected: ExpectedIntegrity,
        size: u64,
        sha1: [u8; 20],
    ) -> Self {
        Self::new(path, is_native, provider_url, expected, size, sha1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExpectedIntegrity {
    pub size: Option<u64>,
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ActualIntegrity {
    pub(super) size: u64,
    pub(super) sha1: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DownloadIntegrityError {
    SizeMismatch {
        file: String,
        expected: u64,
        actual: u64,
    },
    Sha1Mismatch {
        file: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for DownloadIntegrityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "{file} size mismatch: expected {expected}, got {actual}"
            ),
            Self::Sha1Mismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "{file} sha1 mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl ExpectedIntegrity {
    pub fn from_mojang(size: i64, sha1: &str) -> Self {
        Self {
            size: u64::try_from(size).ok().filter(|value| *value > 0),
            sha1: non_empty_sha1(sha1),
        }
    }

    pub fn from_sha1(sha1: &str) -> Self {
        Self {
            size: None,
            sha1: non_empty_sha1(sha1),
        }
    }

    pub fn has_evidence(&self) -> bool {
        self.size.is_some() || self.sha1.is_some()
    }

    pub fn has_checksum(&self) -> bool {
        self.sha1
            .as_deref()
            .is_some_and(super::integrity::is_sha1_hex)
    }
}

fn non_empty_sha1(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionDownloadFactKind {
    ChecksumMismatch,
    MetadataInvalid,
    MetadataMissing,
    Interrupted,
    NetworkFailure,
    PermissionFailure,
    PromoteFailed,
    ProviderFailure,
    SizeMismatch,
    TempDiscarded,
    TempWriteFailed,
    WrittenToTemp,
    Promoted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionDownloadFact {
    pub kind: ExecutionDownloadFactKind,
    pub target: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionDownloadReport {
    pub target: String,
    pub bytes_written: u64,
    pub facts: Vec<ExecutionDownloadFact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectedDownloadArtifactKind {
    VersionJson,
    ClientJar,
    Library,
    AssetIndex,
    AssetObject,
    LogConfig,
}

#[derive(Debug)]
pub struct ExecutionDownloadError {
    pub kind: ExecutionDownloadFactKind,
    pub facts: Vec<ExecutionDownloadFact>,
    pub(super) error: DownloadError,
}

impl ExecutionDownloadError {
    pub fn into_download_error(self) -> DownloadError {
        let Self { kind, facts, error } = self;
        let _fact_report = (kind, facts);
        error
    }
}

pub(super) fn progress(
    phase: &str,
    current: i32,
    total: i32,
    file: Option<String>,
) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}
