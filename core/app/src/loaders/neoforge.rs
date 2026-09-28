//! NeoForge catalog and installer boundary.
//!
//! The retained Minecraft leaf owns numbering, profile authentication, processors,
//! and scoped publication. Catalog records are display data until the installer
//! resolves and compares them with fresh provider authority. A base commit must
//! still be activated and continued by the install owner; it is not readiness.

use axial_minecraft::loaders::{
    self, LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderError, LoaderGameVersion,
    LoaderInstallError, LoaderInstallPublicationOutcome, LoaderVersionIndex,
    providers::neoforge as provider,
};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::{DownloadProgress, ManagedRuntimeCache};

pub const COMPONENT_ID: LoaderComponentId = LoaderComponentId::NeoForge;

/// Resolve the Minecraft identity across both retained NeoForge numbering eras.
/// Snapshot builds with a zero prefix have no supported Minecraft mapping.
pub fn minecraft_version_for(loader_version: &str) -> Option<String> {
    provider::neoforge_to_minecraft_version(loader_version)
}

/// Parse provider metadata without granting installation or filesystem authority.
/// Game targets with beta builds only retain `stable_hint: Some(false)`.
pub fn parse_supported_versions(xml: &str) -> Vec<LoaderGameVersion> {
    provider::parse_game_versions_from_maven_metadata(xml)
}

/// Retain exact upstream build identities and installer artifact coordinates.
/// This provider result has not received the catalog's display normalization.
pub fn parse_builds(minecraft_version: &str, xml: &str) -> Result<LoaderVersionIndex, LoaderError> {
    provider::parse_builds_from_maven_metadata(minecraft_version, xml)
}

pub async fn fetch_supported_versions(
    operation: &ManagedLibraryOperation,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    loaders::fetch_supported_versions(operation, COMPONENT_ID).await
}

pub async fn fetch_builds(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    loaders::fetch_builds(operation, COMPONENT_ID, minecraft_version).await
}

pub fn fetch_cached_builds(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<Option<(Vec<LoaderBuildRecord>, LoaderCatalogState)>, LoaderError> {
    loaders::fetch_cached_builds(operation, COMPONENT_ID, minecraft_version)
}

/// Resolve an opaque selected build from the live provider, including beta builds.
pub async fn resolve_build(build_id: &str) -> Result<LoaderBuildRecord, LoaderError> {
    loaders::resolve_build_record_for_install(COMPONENT_ID, build_id).await
}

/// Begin an exact install, returning the retained publication outcome unchanged.
///
/// The install owner must settle and activate the base commit, consume its opaque
/// continuation with `loaders::continue_install_build_after_base`, and verify and
/// activate the resulting child receipt before exposing readiness. Indeterminate
/// publication errors retain recovery authority and must also remain owned.
/// Terminal leaf callbacks are withheld; only the install owner can publish a
/// terminal result after receipt activation and acknowledgement have settled.
pub async fn install_build<F>(
    operation: &ManagedLibraryOperation,
    runtime_cache: ManagedRuntimeCache,
    record: LoaderBuildRecord,
    mut send: F,
) -> Result<LoaderInstallPublicationOutcome, LoaderInstallError>
where
    F: FnMut(DownloadProgress),
{
    require_neoforge(&record).map_err(LoaderInstallError::from)?;
    loaders::install_build(operation, runtime_cache, record, |progress| {
        forward_pending_progress(progress, &mut send);
    })
    .await
}

fn forward_pending_progress<F>(progress: DownloadProgress, send: &mut F)
where
    F: FnMut(DownloadProgress),
{
    if !progress.done {
        send(progress);
    }
}

fn require_neoforge(record: &LoaderBuildRecord) -> Result<(), LoaderError> {
    if record.component_id == COMPONENT_ID {
        Ok(())
    } else {
        Err(LoaderError::InvalidBuildId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::loaders::{
        LoaderArtifactKind, LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
        LoaderTerm,
    };

    const METADATA: &str = "<metadata><versioning><versions>\
        <version>20.4.239</version>\
        <version>21.0.167</version>\
        <version>21.11.5-beta</version>\
        <version>26.1.0.7-beta</version>\
        <version>26.1.2.10</version>\
        <version>26.1.2.11-beta</version>\
        <version>26.2.0.3-beta</version>\
        <version>26.2.0.4-beta</version>\
        <version>0.25w14craftmine.5-beta</version>\
        </versions></versioning></metadata>";

    #[test]
    fn numbering_eras_map_to_exact_minecraft_targets() {
        for (build, minecraft) in [
            ("20.4.239", "1.20.4"),
            ("21.0.167", "1.21"),
            ("21.11.5-beta", "1.21.11"),
            ("26.1.0.7-beta", "26.1"),
            ("26.1.2.7-beta", "26.1.2"),
        ] {
            assert_eq!(minecraft_version_for(build).as_deref(), Some(minecraft));
        }
        for build in [
            "",
            "0.25w14craftmine.5-beta",
            "0.25.5-beta",
            "beta-26.1.0.1",
        ] {
            assert_eq!(minecraft_version_for(build), None, "{build}");
        }
    }

    #[test]
    fn supported_targets_preserve_beta_only_and_mixed_stability() {
        let mut targets = parse_supported_versions(METADATA)
            .into_iter()
            .map(|version| (version.id, version.stable_hint))
            .collect::<Vec<_>>();
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("1.20.4".to_string(), Some(true)),
                ("1.21".to_string(), Some(true)),
                ("1.21.11".to_string(), Some(false)),
                ("26.1".to_string(), Some(false)),
                ("26.1.2".to_string(), Some(true)),
                ("26.2".to_string(), Some(false)),
            ]
        );
    }

    #[test]
    fn exact_beta_builds_remain_selectable_for_beta_only_targets() {
        let index = parse_builds("26.2", METADATA).expect("beta-only catalog");
        assert_eq!(index.component_id, COMPONENT_ID);
        assert_eq!(
            index
                .builds
                .iter()
                .map(|build| build.loader_version.as_str())
                .collect::<Vec<_>>(),
            vec!["26.2.0.4-beta", "26.2.0.3-beta"]
        );
        for build in index.builds {
            assert_eq!(build.minecraft_version, "26.2");
            assert_eq!(build.installability, LoaderInstallability::Installable);
            assert!(build.build_meta.terms.contains(&LoaderTerm::Beta));
            assert_eq!(
                loaders::parse_build_id(&build.build_id),
                Some((COMPONENT_ID, "26.2".to_string(), build.loader_version))
            );
        }
    }

    #[test]
    fn provider_records_keep_installer_strategy_and_exact_artifact() {
        let index = parse_builds(
            "1.21.5",
            "<metadata><versions><version>21.5.74</version></versions></metadata>",
        )
        .expect("NeoForge catalog");
        let build = &index.builds[0];
        assert_eq!(build.component_id, COMPONENT_ID);
        assert_eq!(build.strategy, LoaderInstallStrategy::NeoForgeModern);
        assert_eq!(build.artifact_kind, LoaderArtifactKind::InstallerJar);
        assert_eq!(
            build.install_source,
            LoaderInstallSource::InstallerJar {
                url: "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.5.74/neoforge-21.5.74-installer.jar".to_string()
            }
        );
        let identity = loaders::validate_materialized_loader_profile(
            &build.version_id,
            &build.version_id,
            "1.21.5",
            true,
        )
        .expect("canonical materialized identity");
        assert_eq!(identity.component_id(), COMPONENT_ID);
        assert_eq!(identity.minecraft_version(), "1.21.5");
        assert_eq!(identity.loader_version(), "21.5.74");
        assert!(
            loaders::validate_materialized_loader_profile(
                &build.version_id,
                &build.version_id,
                "1.21.4",
                true,
            )
            .is_err()
        );
        assert!(
            loaders::validate_materialized_loader_profile(
                &build.version_id,
                &build.version_id,
                "1.21.5",
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn absent_targets_remain_empty_instead_of_falling_back() {
        assert!(
            parse_builds("1.20.1", METADATA)
                .expect("catalog")
                .builds
                .is_empty()
        );
        assert!(
            parse_builds("26.1.0", METADATA)
                .expect("catalog")
                .builds
                .is_empty()
        );
    }

    #[tokio::test]
    async fn foreign_component_install_preserves_the_scoped_library() {
        let directory = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).expect("isolated library");
        let authority =
            axial_minecraft::managed_path::ManagedLibraryTestAuthority::open(directory.path())
                .expect("scoped library authority");
        let runtime = ManagedRuntimeCache::isolated_for_test().expect("isolated runtime");
        let mut record = parse_builds("1.20.4", METADATA)
            .expect("catalog")
            .builds
            .remove(0);
        record.component_id = LoaderComponentId::Forge;
        let before = std::fs::read_dir(directory.path())
            .expect("library entries")
            .count();
        let mut progress_count = 0;
        let outcome = install_build(authority.operation(), runtime, record, |_| {
            progress_count += 1;
        })
        .await;
        assert!(matches!(
            outcome,
            Err(LoaderInstallError::Active(failure))
                if matches!(failure.source(), LoaderError::InvalidBuildId)
        ));
        assert_eq!(progress_count, 0);
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("unchanged library entries")
                .count(),
            before
        );
        assert!(!directory.path().join("versions").exists());
    }

    #[tokio::test]
    async fn invalid_or_foreign_selection_is_rejected_before_provider_io() {
        for build_id in [
            "invalid".to_string(),
            loaders::build_id_for(LoaderComponentId::Forge, "1.21.5", "55.0.0"),
        ] {
            assert!(matches!(
                resolve_build(&build_id).await,
                Err(LoaderError::InvalidBuildId)
            ));
        }
    }

    #[test]
    fn leaf_terminal_events_wait_for_queue_activation_and_acknowledgement() {
        let processors = DownloadProgress {
            phase: "processors".to_string(),
            current: 1,
            total: 2,
            file: Some("processor.jar".to_string()),
            error: None,
            done: false,
            bytes_done: Some(20),
            bytes_total: Some(40),
        };
        let published = DownloadProgress {
            phase: "publication".to_string(),
            current: 2,
            bytes_done: Some(40),
            ..processors.clone()
        };
        let terminal = DownloadProgress {
            phase: "done".to_string(),
            done: true,
            ..published.clone()
        };
        let terminal_error = DownloadProgress {
            error: Some("private installer failure".to_string()),
            ..terminal.clone()
        };
        let mut observed = Vec::new();
        for progress in [
            processors.clone(),
            published.clone(),
            terminal,
            terminal_error,
        ] {
            forward_pending_progress(progress, &mut |progress| observed.push(progress));
        }

        // Fully transferred bytes are still pending while the queue owns the
        // receipt. Neither leaf success nor leaf error may terminalize it.
        assert_eq!(observed, vec![processors, published]);
    }
}
