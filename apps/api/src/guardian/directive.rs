use super::{DiagnosisId, GuardianActionKind};
use axial_launcher::LaunchFailureClass;
use serde::Serialize;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GuardianPresetValue(String);

impl GuardianPresetValue {
    fn normalized(value: &str) -> Self {
        let value = value
            .chars()
            .take(64)
            .filter(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
            .collect::<String>();
        Self(if value.is_empty() {
            "none".to_string()
        } else {
            value
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GuardianPresetValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum GuardianManagedJavaReason {
    Preflight,
    PrepareFailure,
    StartupRecovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum GuardianStripJvmArgsReason {
    Preflight,
    PrepareFailure,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum GuardianPresetDowngradeReason {
    Compatibility {
        requested_preset: GuardianPresetValue,
    },
    StartupRecovery,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum GuardianDirective {
    UseManagedJava {
        reason: GuardianManagedJavaReason,
    },
    StripJvmArgs {
        reason: GuardianStripJvmArgsReason,
    },
    DowngradeJvmPreset {
        preset: GuardianPresetValue,
        reason: GuardianPresetDowngradeReason,
    },
    DisableCustomGc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianRecoveryIntentAxis {
    RequestedJava,
    ExplicitJvmArgs,
    RequestedPreset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GuardianRecoveryMetadata {
    pub(crate) action: GuardianActionKind,
    pub(crate) diagnosis: DiagnosisId,
    pub(crate) intent_axis: GuardianRecoveryIntentAxis,
    pub(crate) fingerprint_tag: &'static [u8],
    pub(crate) step_id: &'static str,
    pub(crate) journal_summary: &'static str,
}

const MANAGED_JAVA_RECOVERY: GuardianRecoveryMetadata = GuardianRecoveryMetadata {
    action: GuardianActionKind::Fallback,
    diagnosis: DiagnosisId::JavaRuntimeRecovery,
    intent_axis: GuardianRecoveryIntentAxis::RequestedJava,
    fingerprint_tag: b"switch_managed_runtime",
    step_id: "launch_recovery_switch_managed_runtime",
    journal_summary: "guardian_launch_recovery_switch_managed_runtime",
};
const STRIP_JVM_ARGS_RECOVERY: GuardianRecoveryMetadata = GuardianRecoveryMetadata {
    action: GuardianActionKind::Strip,
    diagnosis: DiagnosisId::JvmArgUnsupported,
    intent_axis: GuardianRecoveryIntentAxis::ExplicitJvmArgs,
    fingerprint_tag: b"strip_raw_jvm_args",
    step_id: "launch_recovery_strip_raw_jvm_args",
    journal_summary: "guardian_launch_recovery_strip_raw_jvm_args",
};
const DOWNGRADE_PRESET_RECOVERY: GuardianRecoveryMetadata = GuardianRecoveryMetadata {
    action: GuardianActionKind::Downgrade,
    diagnosis: DiagnosisId::JvmPresetRecovery,
    intent_axis: GuardianRecoveryIntentAxis::RequestedPreset,
    fingerprint_tag: b"downgrade_preset",
    step_id: "launch_recovery_downgrade_preset",
    journal_summary: "guardian_launch_recovery_downgrade_preset",
};
const DISABLE_CUSTOM_GC_RECOVERY: GuardianRecoveryMetadata = GuardianRecoveryMetadata {
    action: GuardianActionKind::Strip,
    diagnosis: DiagnosisId::JvmArgUnsupported,
    intent_axis: GuardianRecoveryIntentAxis::RequestedPreset,
    fingerprint_tag: b"disable_custom_gc",
    step_id: "launch_recovery_disable_custom_gc",
    journal_summary: "guardian_launch_recovery_disable_custom_gc",
};

impl GuardianRecoveryMetadata {
    pub(crate) const ALL: [Self; 4] = [
        MANAGED_JAVA_RECOVERY,
        STRIP_JVM_ARGS_RECOVERY,
        DOWNGRADE_PRESET_RECOVERY,
        DISABLE_CUSTOM_GC_RECOVERY,
    ];

    pub(crate) fn supports_failure(self, failure_class: LaunchFailureClass) -> bool {
        match self.diagnosis {
            DiagnosisId::JavaRuntimeRecovery => {
                failure_class == LaunchFailureClass::JavaRuntimeMismatch
            }
            DiagnosisId::JvmArgUnsupported | DiagnosisId::JvmPresetRecovery => matches!(
                failure_class,
                LaunchFailureClass::JvmUnsupportedOption
                    | LaunchFailureClass::JvmExperimentalUnlock
                    | LaunchFailureClass::JvmOptionOrdering
            ),
            _ => false,
        }
    }
}

impl GuardianDirective {
    pub(crate) fn compatibility_preset_downgrade(
        requested_preset: &str,
        effective_preset: &str,
    ) -> Self {
        Self::DowngradeJvmPreset {
            preset: GuardianPresetValue::normalized(effective_preset),
            reason: GuardianPresetDowngradeReason::Compatibility {
                requested_preset: GuardianPresetValue::normalized(requested_preset),
            },
        }
    }

    pub(crate) fn startup_preset_downgrade(preset: &str) -> Self {
        Self::DowngradeJvmPreset {
            preset: GuardianPresetValue::normalized(preset),
            reason: GuardianPresetDowngradeReason::StartupRecovery,
        }
    }

    pub(crate) fn recovery_metadata(&self) -> Option<GuardianRecoveryMetadata> {
        match self {
            Self::UseManagedJava {
                reason:
                    GuardianManagedJavaReason::PrepareFailure
                    | GuardianManagedJavaReason::StartupRecovery,
            } => Some(MANAGED_JAVA_RECOVERY),
            Self::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            } => Some(STRIP_JVM_ARGS_RECOVERY),
            Self::DowngradeJvmPreset {
                reason: GuardianPresetDowngradeReason::StartupRecovery,
                ..
            } => Some(DOWNGRADE_PRESET_RECOVERY),
            Self::DisableCustomGc => Some(DISABLE_CUSTOM_GC_RECOVERY),
            _ => None,
        }
    }

    pub(crate) fn is_prepare_recovery(&self) -> bool {
        matches!(
            self,
            Self::UseManagedJava {
                reason: GuardianManagedJavaReason::PrepareFailure,
            } | Self::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            }
        )
    }

    pub(crate) fn is_startup_recovery(&self) -> bool {
        matches!(
            self,
            Self::UseManagedJava {
                reason: GuardianManagedJavaReason::StartupRecovery,
            } | Self::DowngradeJvmPreset {
                reason: GuardianPresetDowngradeReason::StartupRecovery,
                ..
            } | Self::DisableCustomGc
        )
    }

    pub fn action_kind(&self) -> GuardianActionKind {
        match self {
            Self::UseManagedJava { .. } => GuardianActionKind::Fallback,
            Self::StripJvmArgs { .. } | Self::DisableCustomGc => GuardianActionKind::Strip,
            Self::DowngradeJvmPreset { .. } => GuardianActionKind::Downgrade,
        }
    }

    pub(crate) fn recovery_step_id(&self) -> Option<&'static str> {
        self.recovery_metadata().map(|metadata| metadata.step_id)
    }

    pub(crate) fn recovery_journal_summary(&self) -> Option<&'static str> {
        self.recovery_metadata()
            .map(|metadata| metadata.journal_summary)
    }

    pub(crate) fn recovery_diagnosis(
        &self,
        failure_class: LaunchFailureClass,
    ) -> Option<DiagnosisId> {
        self.recovery_metadata()
            .filter(|metadata| metadata.supports_failure(failure_class))
            .map(|metadata| metadata.diagnosis)
    }
}
