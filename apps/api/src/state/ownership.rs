//! Ownership metadata and protection contracts.
//!
//! State owns target ownership metadata. Guardian and Execution will consume
//! these classifications later when they decide whether a repair is allowed.

use super::contracts::{
    OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind, sanitize_target_id,
};
use axial_minecraft::ManagedRuntimeCache;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnershipProtection {
    AutomaticManagedMutationAllowed,
    ProtectedByDefault,
}

impl OwnershipProtection {
    pub fn allows_automatic_managed_mutation(self) -> bool {
        matches!(self, Self::AutomaticManagedMutationAllowed)
    }

    pub fn is_protected(self) -> bool {
        matches!(self, Self::ProtectedByDefault)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnershipClassification {
    pub target: TargetDescriptor,
    pub protection: OwnershipProtection,
}

impl OwnershipClassification {
    pub fn new(target: TargetDescriptor) -> Self {
        Self {
            protection: protection_for(target.ownership),
            target,
        }
    }

    pub fn allows_automatic_managed_mutation(&self) -> bool {
        self.protection.allows_automatic_managed_mutation()
    }

    pub fn is_protected(&self) -> bool {
        self.protection.is_protected()
    }
}

pub fn protection_for(ownership: OwnershipClass) -> OwnershipProtection {
    match ownership {
        OwnershipClass::LauncherManaged | OwnershipClass::CompositionManaged => {
            OwnershipProtection::AutomaticManagedMutationAllowed
        }
        OwnershipClass::UserOwned
        | OwnershipClass::ExternalProviderDerived
        | OwnershipClass::Unknown => OwnershipProtection::ProtectedByDefault,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CurrentArtifact {
    ManagedRuntimeCache,
    MusicCacheFile,
    BenchmarkSuiteManifest,
    BenchmarkSuiteDriverStatus,
    GuardianFailureMemorySnapshot,
    OperationJournalSnapshot,
    PerformanceRulesCache,
    PerformanceOperationStatus,
    PersistedStateRejectionStreakSnapshot,
    UserJavaOverride,
    UserJvmArguments,
    ExternalPerformanceRules,
    UnknownFilesystemPath,
}

impl CurrentArtifact {
    fn target_system(self) -> StabilizationSystem {
        match self {
            Self::PerformanceRulesCache
            | Self::PerformanceOperationStatus
            | Self::ExternalPerformanceRules => StabilizationSystem::Performance,
            Self::ManagedRuntimeCache | Self::MusicCacheFile => StabilizationSystem::Execution,
            _ => StabilizationSystem::State,
        }
    }

    fn target_kind(self) -> TargetKind {
        match self {
            Self::PerformanceRulesCache
            | Self::GuardianFailureMemorySnapshot
            | Self::OperationJournalSnapshot
            | Self::BenchmarkSuiteManifest
            | Self::BenchmarkSuiteDriverStatus
            | Self::PerformanceOperationStatus => TargetKind::Config,
            Self::PersistedStateRejectionStreakSnapshot => TargetKind::Config,
            Self::UserJavaOverride | Self::UserJvmArguments | Self::UnknownFilesystemPath => {
                TargetKind::FilesystemPath
            }
            Self::ManagedRuntimeCache => TargetKind::Runtime,
            Self::MusicCacheFile => TargetKind::Artifact,
            Self::ExternalPerformanceRules => TargetKind::NetworkResource,
        }
    }

    fn ownership(self) -> OwnershipClass {
        match self {
            Self::ManagedRuntimeCache
            | Self::MusicCacheFile
            | Self::BenchmarkSuiteManifest
            | Self::BenchmarkSuiteDriverStatus
            | Self::GuardianFailureMemorySnapshot
            | Self::OperationJournalSnapshot
            | Self::PerformanceRulesCache
            | Self::PerformanceOperationStatus => OwnershipClass::LauncherManaged,
            Self::PersistedStateRejectionStreakSnapshot => OwnershipClass::LauncherManaged,
            Self::UserJavaOverride | Self::UserJvmArguments => OwnershipClass::UserOwned,
            Self::ExternalPerformanceRules => OwnershipClass::ExternalProviderDerived,
            Self::UnknownFilesystemPath => OwnershipClass::Unknown,
        }
    }

    fn fallback_id(self) -> &'static str {
        match self {
            Self::ManagedRuntimeCache => "managed_runtime_cache",
            Self::MusicCacheFile => "music_cache_file",
            Self::BenchmarkSuiteManifest => "benchmark_suite_manifest",
            Self::BenchmarkSuiteDriverStatus => "benchmark_suite_driver_status",
            Self::GuardianFailureMemorySnapshot => "guardian_failure_memory",
            Self::OperationJournalSnapshot => "operation_journal",
            Self::PerformanceRulesCache => "performance_rules_cache",
            Self::PerformanceOperationStatus => "performance_operation_status",
            Self::PersistedStateRejectionStreakSnapshot => "persisted_state_rejection_streaks",
            Self::UserJavaOverride => "custom_java_path",
            Self::UserJvmArguments => "custom_jvm_args",
            Self::ExternalPerformanceRules => "external_performance_rules",
            Self::UnknownFilesystemPath => "unclassified_path",
        }
    }
}

pub fn classify_current_artifact(
    artifact: CurrentArtifact,
    id: impl AsRef<str>,
) -> OwnershipClassification {
    OwnershipClassification::new(TargetDescriptor::new(
        artifact.target_system(),
        artifact.target_kind(),
        sanitize_target_id(id.as_ref(), artifact.fallback_id()),
        artifact.ownership(),
    ))
}

pub fn classify_managed_runtime_root(
    runtime_cache: &ManagedRuntimeCache,
    runtime_root: &Path,
) -> Option<OwnershipClassification> {
    runtime_cache
        .component_for_root(runtime_root)
        .map(|component| classify_current_artifact(CurrentArtifact::ManagedRuntimeCache, component))
}

#[cfg(test)]
mod tests {
    use super::{CurrentArtifact, OwnershipProtection, classify_current_artifact, protection_for};
    use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetKind};
    use axial_minecraft::ManagedRuntimeCache;

    #[test]
    fn unknown_ownership_is_protected_by_default() {
        let classification =
            classify_current_artifact(CurrentArtifact::UnknownFilesystemPath, "/home/alice/path");

        assert_eq!(classification.target.ownership, OwnershipClass::Unknown);
        assert_eq!(
            classification.protection,
            OwnershipProtection::ProtectedByDefault
        );
        assert!(classification.is_protected());
        assert!(!classification.allows_automatic_managed_mutation());
        assert_eq!(
            protection_for(OwnershipClass::Unknown),
            OwnershipProtection::ProtectedByDefault
        );
    }

    #[test]
    fn current_artifact_classifier_covers_initial_ownership_classes() {
        let artifacts = [
            CurrentArtifact::PerformanceRulesCache,
            CurrentArtifact::UserJvmArguments,
            CurrentArtifact::ExternalPerformanceRules,
            CurrentArtifact::UnknownFilesystemPath,
        ];
        let classes = artifacts
            .into_iter()
            .map(|artifact| classify_current_artifact(artifact, "").target.ownership)
            .collect::<Vec<_>>();

        assert!(classes.contains(&OwnershipClass::LauncherManaged));
        assert!(classes.contains(&OwnershipClass::UserOwned));
        assert!(classes.contains(&OwnershipClass::ExternalProviderDerived));
        assert!(classes.contains(&OwnershipClass::Unknown));
        assert!(
            protection_for(OwnershipClass::LauncherManaged).allows_automatic_managed_mutation()
        );
        assert!(
            protection_for(OwnershipClass::CompositionManaged).allows_automatic_managed_mutation()
        );
        assert!(protection_for(OwnershipClass::UserOwned).is_protected());
        assert!(protection_for(OwnershipClass::ExternalProviderDerived).is_protected());
    }

    #[test]
    fn rejection_streak_snapshot_is_launcher_managed_state_config() {
        let snapshot = classify_current_artifact(
            CurrentArtifact::PersistedStateRejectionStreakSnapshot,
            "persisted_state_rejection_streaks",
        );

        assert_eq!(snapshot.target.system, StabilizationSystem::State);
        assert_eq!(snapshot.target.kind, TargetKind::Config);
        assert_eq!(snapshot.target.ownership, OwnershipClass::LauncherManaged);
        assert!(snapshot.allows_automatic_managed_mutation());
    }

    #[test]
    fn managed_runtime_root_classifier_requires_an_exact_component_root() {
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let runtime_path = runtime_cache
            .component_root("java-runtime-delta")
            .expect("runtime root");
        let global_runtime_root =
            super::classify_managed_runtime_root(&runtime_cache, &runtime_path)
                .expect("managed runtime root");
        assert_eq!(
            global_runtime_root.target.system,
            StabilizationSystem::Execution
        );
        assert_eq!(global_runtime_root.target.kind, TargetKind::Runtime);
        assert_eq!(
            global_runtime_root.target.ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(global_runtime_root.target.id, "java-runtime-delta");

        assert!(
            super::classify_managed_runtime_root(&runtime_cache, runtime_cache.root()).is_none()
        );
        assert!(
            super::classify_managed_runtime_root(&runtime_cache, &runtime_path.join("bin"))
                .is_none()
        );
    }

    #[test]
    fn sensitive_target_ids_fall_back_to_safe_labels() {
        let java_path = classify_current_artifact(
            CurrentArtifact::UserJavaOverride,
            r"C:\Users\Alice\AppData\Local\java.exe",
        );
        let jvm_args = classify_current_artifact(CurrentArtifact::UserJvmArguments, "-Xmx8192M");

        assert_eq!(java_path.target.ownership, OwnershipClass::UserOwned);
        assert_eq!(java_path.target.id, "custom_java_path");
        assert_eq!(jvm_args.target.id, "custom_jvm_args");
    }
}
