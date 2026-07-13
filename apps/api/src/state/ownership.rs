//! Ownership metadata and protection contracts.
//!
//! State owns target ownership metadata. Guardian and Execution will consume
//! these classifications later when they decide whether a repair is allowed.

use super::contracts::{
    OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind, sanitize_target_id,
};
use axial_config::AppPaths;
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
    LauncherConfigFile,
    InstanceRegistryFile,
    ManagedLibraryRoot,
    ManagedRuntimeCache,
    MusicCacheFile,
    InternalLaunchProof,
    BenchmarkSuiteManifest,
    BenchmarkSuiteDriverStatus,
    GuardianFailureMemorySnapshot,
    OperationJournalSnapshot,
    PerformanceRulesCache,
    PerformanceOperationStatus,
    PerformanceCompositionLock,
    ManagedPerformanceArtifact,
    UserWorldDirectory,
    UserScreenshotDirectory,
    UserModsDirectory,
    UserResourcePackDirectory,
    UserShaderPackDirectory,
    UserJavaOverride,
    UserJvmArguments,
    ExternalPerformanceRules,
    ExternalProviderManifest,
    UnknownFilesystemPath,
}

impl CurrentArtifact {
    fn target_system(self) -> StabilizationSystem {
        match self {
            Self::PerformanceRulesCache
            | Self::PerformanceOperationStatus
            | Self::PerformanceCompositionLock
            | Self::ManagedPerformanceArtifact
            | Self::ExternalPerformanceRules => StabilizationSystem::Performance,
            Self::ManagedRuntimeCache | Self::MusicCacheFile | Self::ExternalProviderManifest => {
                StabilizationSystem::Execution
            }
            _ => StabilizationSystem::State,
        }
    }

    fn target_kind(self) -> TargetKind {
        match self {
            Self::LauncherConfigFile
            | Self::InstanceRegistryFile
            | Self::PerformanceRulesCache
            | Self::GuardianFailureMemorySnapshot
            | Self::OperationJournalSnapshot
            | Self::BenchmarkSuiteManifest
            | Self::BenchmarkSuiteDriverStatus
            | Self::PerformanceOperationStatus
            | Self::PerformanceCompositionLock => TargetKind::Config,
            Self::ManagedLibraryRoot
            | Self::UserWorldDirectory
            | Self::UserScreenshotDirectory
            | Self::UserModsDirectory
            | Self::UserResourcePackDirectory
            | Self::UserShaderPackDirectory
            | Self::UserJavaOverride
            | Self::UserJvmArguments
            | Self::UnknownFilesystemPath => TargetKind::FilesystemPath,
            Self::ManagedRuntimeCache => TargetKind::Runtime,
            Self::InternalLaunchProof | Self::MusicCacheFile => TargetKind::Artifact,
            Self::ManagedPerformanceArtifact => TargetKind::Artifact,
            Self::ExternalPerformanceRules | Self::ExternalProviderManifest => {
                TargetKind::NetworkResource
            }
        }
    }

    fn ownership(self) -> OwnershipClass {
        match self {
            Self::LauncherConfigFile
            | Self::InstanceRegistryFile
            | Self::ManagedLibraryRoot
            | Self::ManagedRuntimeCache
            | Self::MusicCacheFile
            | Self::InternalLaunchProof
            | Self::BenchmarkSuiteManifest
            | Self::BenchmarkSuiteDriverStatus
            | Self::GuardianFailureMemorySnapshot
            | Self::OperationJournalSnapshot
            | Self::PerformanceRulesCache
            | Self::PerformanceOperationStatus => OwnershipClass::LauncherManaged,
            Self::PerformanceCompositionLock | Self::ManagedPerformanceArtifact => {
                OwnershipClass::CompositionManaged
            }
            Self::UserWorldDirectory
            | Self::UserScreenshotDirectory
            | Self::UserModsDirectory
            | Self::UserResourcePackDirectory
            | Self::UserShaderPackDirectory
            | Self::UserJavaOverride
            | Self::UserJvmArguments => OwnershipClass::UserOwned,
            Self::ExternalPerformanceRules | Self::ExternalProviderManifest => {
                OwnershipClass::ExternalProviderDerived
            }
            Self::UnknownFilesystemPath => OwnershipClass::Unknown,
        }
    }

    fn fallback_id(self) -> &'static str {
        match self {
            Self::LauncherConfigFile => "launcher_config",
            Self::InstanceRegistryFile => "instance_registry",
            Self::ManagedLibraryRoot => "managed_library",
            Self::ManagedRuntimeCache => "managed_runtime_cache",
            Self::MusicCacheFile => "music_cache_file",
            Self::InternalLaunchProof => "internal_launch_proof",
            Self::BenchmarkSuiteManifest => "benchmark_suite_manifest",
            Self::BenchmarkSuiteDriverStatus => "benchmark_suite_driver_status",
            Self::GuardianFailureMemorySnapshot => "guardian_failure_memory",
            Self::OperationJournalSnapshot => "operation_journal",
            Self::PerformanceRulesCache => "performance_rules_cache",
            Self::PerformanceOperationStatus => "performance_operation_status",
            Self::PerformanceCompositionLock => "performance_composition_lock",
            Self::ManagedPerformanceArtifact => "managed_performance_artifact",
            Self::UserWorldDirectory => "user_world",
            Self::UserScreenshotDirectory => "user_screenshots",
            Self::UserModsDirectory => "user_mods",
            Self::UserResourcePackDirectory => "user_resource_packs",
            Self::UserShaderPackDirectory => "user_shader_packs",
            Self::UserJavaOverride => "custom_java_path",
            Self::UserJvmArguments => "custom_jvm_args",
            Self::ExternalPerformanceRules => "external_performance_rules",
            Self::ExternalProviderManifest => "external_provider_manifest",
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

pub fn classify_app_path(
    paths: &AppPaths,
    runtime_cache: &ManagedRuntimeCache,
    path: &Path,
) -> OwnershipClassification {
    if path == paths.config_file.as_path() {
        return classify_current_artifact(CurrentArtifact::LauncherConfigFile, "");
    }
    if path == paths.instances_file.as_path() {
        return classify_current_artifact(CurrentArtifact::InstanceRegistryFile, "");
    }
    if path == paths.library_dir.as_path() {
        return classify_current_artifact(CurrentArtifact::ManagedLibraryRoot, "");
    }
    if let Some(component) = runtime_cache.component_for_path(path) {
        return classify_current_artifact(CurrentArtifact::ManagedRuntimeCache, component);
    }
    if path.starts_with(&paths.music_dir) {
        return classify_current_artifact(CurrentArtifact::MusicCacheFile, "music_cache_file");
    }
    if path.starts_with(paths.config_dir.join("benchmarks").join("launch")) {
        return classify_current_artifact(CurrentArtifact::InternalLaunchProof, "");
    }
    if path.starts_with(paths.config_dir.join("benchmarks").join("suites")) {
        return classify_current_artifact(CurrentArtifact::BenchmarkSuiteManifest, "");
    }
    if path.starts_with(paths.config_dir.join("benchmarks").join("suite-drivers")) {
        return classify_current_artifact(CurrentArtifact::BenchmarkSuiteDriverStatus, "");
    }
    if path.starts_with(paths.config_dir.join("performance")) {
        return classify_current_artifact(CurrentArtifact::PerformanceCompositionLock, "");
    }
    if path.starts_with(&paths.instances_dir) {
        return classify_current_artifact(
            CurrentArtifact::UnknownFilesystemPath,
            "instance_filesystem",
        );
    }

    classify_current_artifact(CurrentArtifact::UnknownFilesystemPath, "")
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
    use super::{
        CurrentArtifact, OwnershipProtection, classify_app_path, classify_current_artifact,
        protection_for,
    };
    use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetKind};
    use axial_config::AppPaths;
    use axial_minecraft::ManagedRuntimeCache;
    use std::path::PathBuf;

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
            CurrentArtifact::LauncherConfigFile,
            CurrentArtifact::PerformanceCompositionLock,
            CurrentArtifact::UserJvmArguments,
            CurrentArtifact::ExternalPerformanceRules,
            CurrentArtifact::UnknownFilesystemPath,
        ];
        let classes = artifacts
            .into_iter()
            .map(|artifact| classify_current_artifact(artifact, "").target.ownership)
            .collect::<Vec<_>>();

        assert!(classes.contains(&OwnershipClass::LauncherManaged));
        assert!(classes.contains(&OwnershipClass::CompositionManaged));
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
    fn app_path_classifier_uses_safe_ids_and_protects_unknown_instance_paths() {
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let root = PathBuf::from("/tmp/axial-test");
        let paths = AppPaths {
            config_file: root.join("config").join("config.json"),
            instances_file: root.join("config").join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir: root.join("config"),
        };

        let config = classify_app_path(&paths, &runtime_cache, &paths.config_file);
        assert_eq!(config.target.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(config.target.id, "launcher_config");
        assert!(config.allows_automatic_managed_mutation());

        let runtime_path = runtime_cache
            .component_root("java-runtime-delta")
            .expect("runtime root");
        let runtime = classify_app_path(&paths, &runtime_cache, &runtime_path);
        assert_eq!(runtime.target.system, StabilizationSystem::Execution);
        assert_eq!(runtime.target.kind, TargetKind::Runtime);
        assert_eq!(runtime.target.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(runtime.target.id, "java-runtime-delta");
        assert!(!runtime.target.id.contains('/'));

        let runtime_child = classify_app_path(
            &paths,
            &runtime_cache,
            &runtime_path.join("bin").join("java"),
        );
        assert_eq!(runtime_child.target.id, "java-runtime-delta");

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
        let music = classify_app_path(&paths, &runtime_cache, &paths.music_dir.join("track.mp3"));
        assert_eq!(music.target.system, StabilizationSystem::Execution);
        assert_eq!(music.target.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(music.target.id, "music_cache_file");
        assert!(music.allows_automatic_managed_mutation());

        let driver_status = classify_app_path(
            &paths,
            &runtime_cache,
            &paths
                .config_dir
                .join("benchmarks")
                .join("suite-drivers")
                .join("benchmark-suite-driver-0000000000000001.json"),
        );
        assert_eq!(driver_status.target.system, StabilizationSystem::State);
        assert_eq!(driver_status.target.kind, TargetKind::Config);
        assert_eq!(
            driver_status.target.ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(driver_status.target.id, "benchmark_suite_driver_status");

        let suite_manifest = classify_app_path(
            &paths,
            &runtime_cache,
            &paths
                .config_dir
                .join("benchmarks")
                .join("suites")
                .join("suite-development.json"),
        );
        assert_eq!(suite_manifest.target.system, StabilizationSystem::State);
        assert_eq!(suite_manifest.target.kind, TargetKind::Config);
        assert_eq!(
            suite_manifest.target.ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(suite_manifest.target.id, "benchmark_suite_manifest");
        assert!(suite_manifest.allows_automatic_managed_mutation());

        let instance_path = classify_app_path(
            &paths,
            &runtime_cache,
            &paths
                .instances_dir
                .join("example-instance")
                .join("saves")
                .join("world"),
        );
        assert_eq!(instance_path.target.ownership, OwnershipClass::Unknown);
        assert!(instance_path.is_protected());
        assert_eq!(instance_path.target.id, "instance_filesystem");
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
