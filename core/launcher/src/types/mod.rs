use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }
    };
}

id_type!(InstanceId);
id_type!(SessionId);
id_type!(VersionId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchState {
    Idle,
    Queued,
    Planning,
    Validating,
    EnsuringRuntime,
    DownloadingRuntime,
    Preparing,
    Starting,
    Monitoring,
    Running,
    Degraded,
    Failed,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchFailureClass {
    Unknown,
    JvmUnsupportedOption,
    JvmExperimentalUnlock,
    JvmOptionOrdering,
    JavaRuntimeMismatch,
    ClasspathModuleConflict,
    AuthModeIncompatible,
    LoaderBootstrapFailure,
    StartupStalled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchFailure {
    pub class: LaunchFailureClass,
    #[serde(default)]
    pub detail: Option<String>,
}
