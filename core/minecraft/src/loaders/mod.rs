pub mod api;
pub mod artifacts;
mod compose;
mod forge_installer;
mod http;
pub mod index;
mod installed_metadata;
pub mod legacy;
mod managed_fs;
mod processors;
pub mod providers;
mod source;
pub mod strategies;
pub mod types;
pub mod workspace;

pub use api::{build_id_for, installed_version_id_for, loader_components, parse_build_id};
pub use index::{
    fetch_builds, fetch_cached_builds, fetch_components, fetch_supported_versions,
    resolve_build_record_for_install,
};
pub(crate) use installed_metadata::{
    InstalledLoaderMetadata, installed_loader_metadata_bytes,
    materialized_profile_has_valid_provenance,
};
pub use installed_metadata::{InstalledLoaderProvenance, validated_installed_loader_provenance};
pub use types::{
    LOADER_CATALOG_SCHEMA_VERSION, LoaderActiveInstallFailure, LoaderArtifactKind,
    LoaderAvailability, LoaderBuildId, LoaderBuildMetadata, LoaderBuildRecord, LoaderCatalogState,
    LoaderComponentId, LoaderComponentRecord, LoaderError, LoaderGameVersion, LoaderInstallError,
    LoaderInstallFailureKind, LoaderInstallPlan, LoaderInstallSource, LoaderInstallStrategy,
    LoaderInstallability, LoaderPreOperationFailureKind, LoaderProviderFailureKind,
    LoaderSelectionMeta, LoaderSelectionReason, LoaderSelectionSource, LoaderTerm,
    LoaderTermEvidence, LoaderTermSource, LoaderVersionIndex,
};

use crate::artifact_path::MAX_ARTIFACT_PATH_SEGMENT_BYTES;
use crate::download::DownloadProgress;
use crate::known_good::KnownGoodInstallReceipt;
use std::path::{Component, Path};

pub(crate) const MAX_VERSION_ID_BYTES: usize = MAX_ARTIFACT_PATH_SEGMENT_BYTES - ".json".len();

#[derive(Debug, Eq, PartialEq)]
pub enum LoaderInstallOutcome {
    KnownGood(Box<KnownGoodInstallReceipt>),
    PendingAuthority { version_id: String },
}

impl LoaderInstallOutcome {
    pub fn version_id(&self) -> &str {
        match self {
            Self::KnownGood(receipt) => receipt.version_id(),
            Self::PendingAuthority { version_id } => version_id,
        }
    }
}

pub async fn install_build<F>(
    library_dir: &Path,
    record: LoaderBuildRecord,
    send: F,
) -> Result<LoaderInstallOutcome, LoaderInstallError>
where
    F: FnMut(DownloadProgress),
{
    api::validate_loader_build_record_identity(&record).map_err(LoaderInstallError::from)?;
    validate_version_id(&record.version_id, "loader build version id")
        .map_err(LoaderInstallError::from)?;
    let live_record = resolve_build_record_for_install(record.component_id, &record.build_id)
        .await
        .map_err(LoaderInstallError::from)?;
    let record =
        require_exact_live_build_record(&record, live_record).map_err(LoaderInstallError::from)?;
    let plan = LoaderInstallPlan { record };
    Box::pin(strategies::install_build(library_dir, &plan, send))
        .await
        .map_err(LoaderInstallError::from)
}

fn require_exact_live_build_record(
    requested: &LoaderBuildRecord,
    live: LoaderBuildRecord,
) -> Result<LoaderBuildRecord, LoaderError> {
    if requested != &live {
        return Err(LoaderError::InvalidProfile(
            "requested loader build does not match live provider authority".to_string(),
        ));
    }
    Ok(live)
}

pub(crate) fn validate_version_id(version_id: &str, context: &str) -> Result<(), LoaderError> {
    validate_version_id_shape(version_id, context).map_err(LoaderError::InstallExecutionFailed)
}

pub(crate) fn validate_provider_version_id(
    version_id: &str,
    context: &str,
) -> Result<(), LoaderError> {
    validate_version_id_shape(version_id, context).map_err(LoaderError::InvalidProfile)
}

fn validate_version_id_shape(version_id: &str, context: &str) -> Result<(), String> {
    let trimmed = version_id.trim();
    if trimmed.is_empty() {
        return Err(format!("{context} is empty"));
    }
    if version_id != trimmed {
        return Err(format!("{context} contains surrounding whitespace"));
    }
    if version_id.len() > MAX_VERSION_ID_BYTES {
        return Err(format!("{context} is too long"));
    }
    if version_id.contains(':') || version_id.chars().any(char::is_control) {
        return Err(format!("{context} is not a portable path segment"));
    }
    if trimmed.contains(['/', '\\']) {
        return Err(format!("{context} contains path separators"));
    }
    let mut components = Path::new(trimmed).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!("{context} is invalid"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderComponentId, LoaderError,
        LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability, MAX_VERSION_ID_BYTES,
        build_id_for, install_build, installed_version_id_for, require_exact_live_build_record,
        validate_version_id,
    };
    use crate::loaders::types::LoaderBuildSubjectKind;
    use crate::paths::loader_work_dir;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rejects_empty_version_ids() {
        let error = validate_version_id(" \t ", "loader build version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "loader build version id is empty"
        ));
    }

    #[test]
    fn rejects_path_traversal_version_ids() {
        for version_id in ["..", ".", "../escape", "loader/escape", "loader\\escape"] {
            let error =
                validate_version_id(version_id, "loader build version id").expect_err("error");
            assert!(
                error.to_string().contains("loader build version id"),
                "{version_id} => {}",
                error
            );
        }
    }

    #[test]
    fn rejects_whitespace_padded_version_ids() {
        let error =
            validate_version_id(" loader-id ", "loader build version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "loader build version id contains surrounding whitespace"
        ));
    }

    #[test]
    fn rejects_ids_whose_json_filename_exceeds_known_good_segment_limit() {
        let version_id = "a".repeat(MAX_VERSION_ID_BYTES + 1);
        let error =
            validate_version_id(&version_id, "loader build version id").expect_err("oversized id");

        assert!(error.to_string().contains("too long"));
    }

    #[tokio::test]
    async fn install_rejects_noncanonical_identity_before_creating_workspace() {
        let root = temp_library("noncanonical-install-identity");
        let component_id = LoaderComponentId::Fabric;
        let version_id = installed_version_id_for(component_id, "1.21.5", "0.16.14")
            .expect("canonical installed version id");
        let mut build_id = build_id_for(component_id, "1.21.5", "0.16.14");
        build_id.push('A');
        let record = LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id,
            minecraft_version: "1.21.5".to_string(),
            loader_version: "0.16.14".to_string(),
            version_id,
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://example.invalid/profile.json".to_string(),
            },
        };

        install_build(&root, record, |_| {})
            .await
            .expect_err("noncanonical identity");

        assert!(!loader_work_dir(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_build_gate_rejects_canonical_records_with_caller_mutations() {
        let component_id = LoaderComponentId::Fabric;
        let loader_version = "0.16.14";
        let minecraft_version = "1.21.5";
        let live = LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: super::build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
            version_id: installed_version_id_for(component_id, minecraft_version, loader_version)
                .expect("canonical installed version id"),
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://meta.fabricmc.net/official-profile.json".to_string(),
            },
        };

        let mut attacker_source = live.clone();
        attacker_source.install_source = LoaderInstallSource::ProfileJson {
            url: "https://attacker.invalid/profile.json".to_string(),
        };
        assert!(
            require_exact_live_build_record(&attacker_source, live.clone()).is_err(),
            "caller-selected source must not mint install authority"
        );

        let mut mutated_metadata = live.clone();
        mutated_metadata.component_name = "Caller Fabric".to_string();
        assert!(
            require_exact_live_build_record(&mutated_metadata, live.clone()).is_err(),
            "all caller-supplied record fields must match live authority"
        );

        assert_eq!(
            require_exact_live_build_record(&live, live.clone()).expect("exact live record"),
            live
        );
    }

    fn temp_library(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-loader-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }
}
