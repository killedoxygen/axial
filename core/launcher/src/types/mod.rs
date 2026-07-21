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

id_type!(SessionId);

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
    Recovering,
    Running,
    Degraded,
    Settling,
    Failed,
    Exited,
}

macro_rules! launch_failure_classes {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum LaunchFailureClass {
            $($variant),+
        }

        impl LaunchFailureClass {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }

            pub fn from_name(raw: &str) -> Option<Self> {
                match raw {
                    $($wire => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

launch_failure_classes! {
    Unknown => "unknown",
    JvmUnsupportedOption => "jvm_unsupported_option",
    JvmExperimentalUnlock => "jvm_experimental_unlock",
    JvmOptionOrdering => "jvm_option_ordering",
    JavaRuntimeMismatch => "java_runtime_mismatch",
    OutOfMemory => "out_of_memory",
    GraphicsDriverCrash => "graphics_driver_crash",
    MissingDependency => "missing_dependency",
    ModTransformationFailure => "mod_transformation_failure",
    ModAttributedCrash => "mod_attributed_crash",
    ClasspathModuleConflict => "classpath_module_conflict",
    LauncherManagedArtifactSignature => "launcher_managed_artifact_signature",
    AuthModeIncompatible => "auth_mode_incompatible",
    LoaderBootstrapFailure => "loader_bootstrap_failure",
    StartupStalled => "startup_stalled",
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
        for &class in LaunchFailureClass::ALL {
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
