//! Runtime wire compatibility and explicit selection policy.

pub use axial_minecraft::{
    JavaRuntimeInfo, JavaRuntimeResult, JavaVersion, ManagedRuntimeCache,
    ManagedRuntimeLaunchReceipt, RuntimeEnsureEvent, RuntimeEnsureResult, RuntimeId,
    RuntimeInstallState, RuntimeOverride, RuntimeRecord, RuntimeRequirement, RuntimeSource,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JavaRuntimesResponse {
    pub runtimes: Vec<JavaRuntimeResult>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JavaArchitecture {
    X86,
    X86_64,
    Aarch64,
    Unknown,
}

impl JavaArchitecture {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "x86" | "i386" | "i486" | "i586" | "i686" => Self::X86,
            "amd64" | "x86_64" | "x64" => Self::X86_64,
            "aarch64" | "arm64" => Self::Aarch64,
            _ => Self::Unknown,
        }
    }
}

/// Exact Minecraft major and native architecture unless a successful macOS
/// probe has already demonstrated the retained x86_64 translation path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JavaSelectionRequirement {
    pub major: u32,
    pub architecture: JavaArchitecture,
    pub allow_macos_x86_64_translation: bool,
}

impl JavaSelectionRequirement {
    pub fn for_current_host(major: u32) -> Self {
        Self {
            major,
            architecture: JavaArchitecture::parse(std::env::consts::ARCH),
            allow_macos_x86_64_translation: cfg!(all(target_os = "macos", target_arch = "aarch64")),
        }
    }

    pub fn validate(
        &self,
        info: &JavaRuntimeInfo,
        architecture: JavaArchitecture,
    ) -> Result<(), JavaDiscoveryError> {
        if self.major == 0 || info.major == 0 {
            return Err(JavaDiscoveryError::InvalidVersion);
        }
        if info.major != self.major {
            return Err(JavaDiscoveryError::IncompatibleVersion {
                required: self.major,
                actual: info.major,
            });
        }
        // The baseline's hard Java 8 floor belongs to runtime validation.
        if self.major == 8 && info.update < 312 {
            return Err(JavaDiscoveryError::OutdatedJava8);
        }
        if architecture == JavaArchitecture::Unknown || self.architecture == JavaArchitecture::Unknown {
            return Err(JavaDiscoveryError::UnknownArchitecture);
        }
        let translated = self.allow_macos_x86_64_translation
            && self.architecture == JavaArchitecture::Aarch64
            && architecture == JavaArchitecture::X86_64;
        if architecture != self.architecture && !translated {
            return Err(JavaDiscoveryError::IncompatibleArchitecture {
                required: self.architecture,
                actual: architecture,
            });
        }
        Ok(())
    }
}

/// Bounded public failure vocabulary. Raw paths, process output and OS errors
/// are deliberately excluded from user-facing error text.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum JavaDiscoveryError {
    #[error("The selected Java executable is missing.")]
    Missing,
    #[error("Select an absolute path to a Java executable.")]
    InvalidPath,
    #[error("The selected Java file is not an executable regular file.")]
    NotExecutable,
    #[error("The selected Java executable exceeds the inspection limit.")]
    ExecutableTooLarge,
    #[error("The selected Java executable changed. Select it again.")]
    Replaced,
    #[error("Java did not respond before the probe deadline.")]
    TimedOut,
    #[error("Java produced too much diagnostic output.")]
    OutputLimit,
    #[error("Java did not report a valid version.")]
    InvalidVersion,
    #[error("Java did not report a recognized architecture.")]
    UnknownArchitecture,
    #[error("The selected Java executable could not run.")]
    Failed,
    #[error("The Java probe was cancelled.")]
    Cancelled,
    #[error("Java {required} is required; the selected executable provides Java {actual}.")]
    IncompatibleVersion { required: u32, actual: u32 },
    #[error("Java 8 update 312 or newer is required.")]
    OutdatedJava8,
    #[error("The selected Java architecture is incompatible with this launch.")]
    IncompatibleArchitecture { required: JavaArchitecture, actual: JavaArchitecture },
    #[error("The selected Java runtime requires Rosetta 2 on this Mac.")]
    RosettaRequired,
    #[error("The managed Java runtime is not ready.")]
    ManagedNotReady,
    #[error("Managed Java runtime acquisition failed: {}.", .0.as_str())]
    ManagedSource(axial_minecraft::runtime::RuntimeSourceFailureKind),
    #[error("The managed Java runtime is unavailable for this platform.")]
    ManagedUnsupportedPlatform,
    #[error("Managed Java runtime file changes have not settled.")]
    ManagedSettlementRequired,
    #[error("The Java runtime library is unavailable.")]
    LibraryUnavailable,
    #[error("The Java probe process has not finished shutting down.")]
    SettlementRequired,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(major: u32, update: u32) -> JavaRuntimeInfo {
        JavaRuntimeInfo { id: String::new(), major, update, distribution: "temurin".into(), path: String::new() }
    }

    #[test]
    fn selection_enforces_major_update_and_architecture() {
        let request = JavaSelectionRequirement { major: 8, architecture: JavaArchitecture::Aarch64, allow_macos_x86_64_translation: false };
        assert_eq!(request.validate(&info(8, 311), JavaArchitecture::Aarch64), Err(JavaDiscoveryError::OutdatedJava8));
        assert!(request.validate(&info(8, 312), JavaArchitecture::Aarch64).is_ok());
        assert!(matches!(request.validate(&info(17, 10), JavaArchitecture::Aarch64), Err(JavaDiscoveryError::IncompatibleVersion { .. })));
        assert!(matches!(request.validate(&info(8, 312), JavaArchitecture::X86_64), Err(JavaDiscoveryError::IncompatibleArchitecture { .. })));
        assert_eq!(request.validate(&info(8, 312), JavaArchitecture::Unknown), Err(JavaDiscoveryError::UnknownArchitecture));
        assert!(JavaSelectionRequirement { allow_macos_x86_64_translation: true, ..request }.validate(&info(8, 312), JavaArchitecture::X86_64).is_ok());
    }

    #[test]
    fn runtime_response_keeps_existing_wire_shape() {
        let value = JavaRuntimesResponse { runtimes: vec![JavaRuntimeResult { path: "/java/bin/java".into(), component: "java-runtime-delta".into(), source: "managed".into() }] };
        assert_eq!(serde_json::to_value(value).unwrap(), serde_json::json!({"runtimes":[{"path":"/java/bin/java","component":"java-runtime-delta","source":"managed"}]}));
    }
}
