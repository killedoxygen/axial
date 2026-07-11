use serde::{Deserialize, Serialize};
use std::str::FromStr;

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
    Prewarming,
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
    OutOfMemory,
    ClasspathModuleConflict,
    LauncherManagedArtifactSignature,
    AuthModeIncompatible,
    LoaderBootstrapFailure,
    StartupStalled,
}

impl LaunchFailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::JvmUnsupportedOption => "jvm_unsupported_option",
            Self::JvmExperimentalUnlock => "jvm_experimental_unlock",
            Self::JvmOptionOrdering => "jvm_option_ordering",
            Self::JavaRuntimeMismatch => "java_runtime_mismatch",
            Self::OutOfMemory => "out_of_memory",
            Self::ClasspathModuleConflict => "classpath_module_conflict",
            Self::LauncherManagedArtifactSignature => "launcher_managed_artifact_signature",
            Self::AuthModeIncompatible => "auth_mode_incompatible",
            Self::LoaderBootstrapFailure => "loader_bootstrap_failure",
            Self::StartupStalled => "startup_stalled",
        }
    }

    pub fn from_name(raw: &str) -> Option<Self> {
        Some(match raw {
            "unknown" => Self::Unknown,
            "jvm_unsupported_option" => Self::JvmUnsupportedOption,
            "jvm_experimental_unlock" => Self::JvmExperimentalUnlock,
            "jvm_option_ordering" => Self::JvmOptionOrdering,
            "java_runtime_mismatch" => Self::JavaRuntimeMismatch,
            "out_of_memory" => Self::OutOfMemory,
            "classpath_module_conflict" => Self::ClasspathModuleConflict,
            "launcher_managed_artifact_signature" => Self::LauncherManagedArtifactSignature,
            "auth_mode_incompatible" => Self::AuthModeIncompatible,
            "loader_bootstrap_failure" => Self::LoaderBootstrapFailure,
            "startup_stalled" => Self::StartupStalled,
            _ => return None,
        })
    }
}

impl FromStr for LaunchFailureClass {
    type Err = ();

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::from_name(raw).ok_or(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchFailure {
    pub class: LaunchFailureClass,
    #[serde(default)]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::LaunchFailureClass;

    #[test]
    fn launch_failure_class_strings_match_serde_names() {
        for class in [
            LaunchFailureClass::Unknown,
            LaunchFailureClass::JvmUnsupportedOption,
            LaunchFailureClass::JvmExperimentalUnlock,
            LaunchFailureClass::JvmOptionOrdering,
            LaunchFailureClass::JavaRuntimeMismatch,
            LaunchFailureClass::OutOfMemory,
            LaunchFailureClass::ClasspathModuleConflict,
            LaunchFailureClass::LauncherManagedArtifactSignature,
            LaunchFailureClass::AuthModeIncompatible,
            LaunchFailureClass::LoaderBootstrapFailure,
            LaunchFailureClass::StartupStalled,
        ] {
            let serialized = serde_json::to_string(&class).expect("serialize");
            assert_eq!(serialized.trim_matches('"'), class.as_str());
            assert_eq!(
                serde_json::from_str::<LaunchFailureClass>(&serialized).expect("deserialize"),
                class
            );
            assert_eq!(LaunchFailureClass::from_name(class.as_str()), Some(class));
        }
    }
}
