use crate::launch::JavaVersion;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeProbeSource {
    #[default]
    None,
    Fresh,
    Receipt,
    FreshAfterReceiptMismatch,
}

impl RuntimeProbeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fresh => "fresh",
            Self::Receipt => "receipt",
            Self::FreshAfterReceiptMismatch => "fresh_after_receipt_mismatch",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeProbeUsage {
    pub spawn_count: u8,
    pub source: RuntimeProbeSource,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEnsureResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<RuntimeRecord>,
    pub effective: RuntimeRecord,
    #[serde(skip)]
    pub probe_usage: RuntimeProbeUsage,
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
    Source(String),
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
}
