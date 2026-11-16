use crate::launch::JavaVersion;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRuntimeInfo {
    pub id: String,
    pub major: u32,
    #[serde(default)]
    pub update: u32,
    #[serde(default)]
    pub distribution: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct JavaRuntimeResult {
    pub path: String,
    pub component: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeId(pub String);

impl RuntimeId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for RuntimeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for RuntimeId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSourceFailureKind {
    Unavailable,
    MetadataInvalid,
    IntegrityMismatch,
    PolicyRejected,
}

impl RuntimeSourceFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::MetadataInvalid => "metadata_invalid",
            Self::IntegrityMismatch => "integrity_mismatch",
            Self::PolicyRejected => "policy_rejected",
        }
    }

    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSourceFailure {
    component: RuntimeId,
    kind: RuntimeSourceFailureKind,
    detail: String,
}

impl RuntimeSourceFailure {
    pub fn new(
        component: RuntimeId,
        kind: RuntimeSourceFailureKind,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            component,
            kind,
            detail: detail.into(),
        }
    }

    pub fn component(&self) -> &RuntimeId {
        &self.component
    }

    pub const fn kind(&self) -> RuntimeSourceFailureKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for RuntimeSourceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    Managed,
    ExternalOverride,
}

impl RuntimeSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::ExternalOverride => "override",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeInstallState {
    Missing,
    Ready,
    Broken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub id: RuntimeId,
    pub java_path: String,
    pub info: JavaRuntimeInfo,
    pub source: RuntimeSource,
    pub install_state: RuntimeInstallState,
    pub root_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRequirement {
    pub required_java: JavaVersion,
    pub preferred_component: RuntimeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeOverride {
    None,
    Component(RuntimeId),
    ExecutablePath(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEnsureEvent {
    DownloadingManagedRuntime {
        component: String,
    },
    InstallingManagedRuntimeFiles {
        component: String,
        current: usize,
        total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    ManagedRuntimeReady {
        component: String,
    },
}

#[derive(Debug, Error)]
pub enum JavaRuntimeLookupError {
    #[error("java runtime not found: {component} (Java {major}) not installed")]
    NotFound { component: String, major: i32 },
    #[error("failed to install java runtime: {0}")]
    Install(String),
    #[error("failed to acquire java runtime source: {0}")]
    RuntimeSource(RuntimeSourceFailure),
    #[error("java runtime {component} is not available for {platform}")]
    UnsupportedPlatform { component: String, platform: String },
    #[error(
        "java runtime {component} needs Rosetta 2 on this Mac: run `softwareupdate --install-rosetta --agree-to-license` in Terminal"
    )]
    RosettaRequired { component: String },
    #[error("java runtime probe timed out")]
    ProbeTimedOut,
    #[error("failed to probe java runtime: {0}")]
    Probe(String),
    #[error("managed runtime mutation was refused before effects")]
    ManagedMutationRefused,
}

/// Java 8 builds older than this update cannot run supported versions safely.
pub const JAVA_8_MINIMUM_UPDATE: u32 = 312;

/// The Java a launch or loader processor may run, and the build managed
/// acquisition may provision: the exact major, Java 8 at update
/// [`JAVA_8_MINIMUM_UPDATE`] or newer, and the host architecture or x86_64
/// where this host translates it (Rosetta 2 on Apple silicon, x64 emulation on
/// Windows on Arm).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JavaRequirement {
    pub major: u32,
    pub architecture: super::probe::JavaArchitecture,
    pub translates_x86_64: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JavaRequirementRefusal {
    InvalidVersion,
    IncompatibleVersion { required: u32, actual: u32 },
    OutdatedJava8,
    UnknownArchitecture,
    IncompatibleArchitecture {
        required: super::probe::JavaArchitecture,
        actual: super::probe::JavaArchitecture,
    },
}

impl JavaRequirement {
    pub fn for_current_host(major: u32) -> Self {
        Self {
            major,
            architecture: super::probe::JavaArchitecture::parse(std::env::consts::ARCH),
            translates_x86_64: cfg!(all(
                any(target_os = "macos", target_os = "windows"),
                target_arch = "aarch64"
            )),
        }
    }

    /// The version alone, as published metadata states it before download.
    pub fn admits_version(&self, major: u32, update: u32) -> Result<(), JavaRequirementRefusal> {
        if self.major == 0 || major == 0 {
            return Err(JavaRequirementRefusal::InvalidVersion);
        }
        if major != self.major {
            return Err(JavaRequirementRefusal::IncompatibleVersion {
                required: self.major,
                actual: major,
            });
        }
        if major == 8 && update < JAVA_8_MINIMUM_UPDATE {
            return Err(JavaRequirementRefusal::OutdatedJava8);
        }
        Ok(())
    }

    pub fn admits_architecture(
        &self,
        architecture: super::probe::JavaArchitecture,
    ) -> Result<(), JavaRequirementRefusal> {
        use super::probe::JavaArchitecture;
        if architecture == JavaArchitecture::Unknown || self.architecture == JavaArchitecture::Unknown
        {
            return Err(JavaRequirementRefusal::UnknownArchitecture);
        }
        let translated = self.translates_x86_64
            && self.architecture == JavaArchitecture::Aarch64
            && architecture == JavaArchitecture::X86_64;
        if architecture != self.architecture && !translated {
            return Err(JavaRequirementRefusal::IncompatibleArchitecture {
                required: self.architecture,
                actual: architecture,
            });
        }
        Ok(())
    }

    /// A probed runtime: its reported version and architecture.
    pub fn validate(
        &self,
        info: &JavaRuntimeInfo,
        architecture: super::probe::JavaArchitecture,
    ) -> Result<(), JavaRequirementRefusal> {
        self.admits_version(info.major, info.update)?;
        self.admits_architecture(architecture)
    }
}
