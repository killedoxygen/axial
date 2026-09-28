//! Forge admission around the retained Minecraft installer.
//!
//! Archive overlays, legacy FML installers and modern processors all remain in
//! the same leaf implementation. A prepared selection authenticates no bytes and
//! grants no filesystem authority: installation still resolves the live build,
//! admits its source and publishes through the retained library operation.

use axial_minecraft::download::DownloadProgress;
use axial_minecraft::loaders::api::validate_loader_build_record_identity;
use axial_minecraft::loaders::providers::forge_install_source;
use axial_minecraft::loaders::{
    self, LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderError, LoaderGameVersion,
    LoaderInstallError, LoaderInstallPublicationOutcome,
};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::runtime::ManagedRuntimeCache;

pub const COMPONENT_ID: LoaderComponentId = LoaderComponentId::Forge;

/// A structurally checked, immutable Forge selection. This is deliberately not
/// deserializable; external records must pass `prepare` again.
#[derive(Clone, Debug)]
#[must_use]
pub struct PreparedForgeBuild {
    record: LoaderBuildRecord,
}

impl PreparedForgeBuild {
    pub fn prepare(record: LoaderBuildRecord) -> Result<Self, LoaderError> {
        validate_record(&record)?;
        Ok(Self { record })
    }

    pub fn record(&self) -> &LoaderBuildRecord {
        &self.record
    }

    /// The install coordinator may use the shared loader admission boundary,
    /// which independently checks this record against the fresh catalog.
    pub fn into_record(self) -> LoaderBuildRecord {
        self.record
    }

    /// Begin the retained base/child protocol under the caller's admitted root.
    ///
    /// `BaseCommitted` is a checkpoint, not a completed Forge installation. The
    /// coordinator must activate that exact receipt before consuming its child
    /// continuation. Publication recovery is returned intact, with its retained
    /// root authority. Accepted work must remain owned until it settles; dropping
    /// a request waiter must not turn a late publication into cancellation.
    pub fn install_base<'a, F>(
        self,
        library_root: &'a ManagedLibraryOperation,
        runtime_cache: ManagedRuntimeCache,
        mut send: F,
    ) -> impl std::future::Future<
        Output = Result<LoaderInstallPublicationOutcome, LoaderInstallError>,
    > + 'a
    where
        F: FnMut(DownloadProgress) + 'a,
    {
        loaders::install_build(library_root, runtime_cache, self.record, move |progress| {
            forward_pending_progress(progress, &mut send);
        })
    }
}

/// Preserve all provider strategies; callers cannot replace the artifact URL or
/// downgrade a modern processor build to an archive with the same identity.
pub fn validate_record(record: &LoaderBuildRecord) -> Result<(), LoaderError> {
    if record.component_id != LoaderComponentId::Forge {
        return Err(LoaderError::InvalidBuildId);
    }
    validate_loader_build_record_identity(record)?;
    let (strategy, artifact_kind, install_source) =
        forge_install_source(&record.minecraft_version, &record.loader_version)?;
    if record.strategy != strategy
        || record.artifact_kind != artifact_kind
        || record.install_source != install_source
    {
        return Err(LoaderError::InvalidProfile(
            "Forge source does not match the selected build".to_string(),
        ));
    }
    Ok(())
}

/// Fetch provider rows without imposing a Minecraft version floor. Manifest
/// ordering, freshness and release metadata belong to the catalog owner.
pub async fn fetch_supported_versions(
    operation: &ManagedLibraryOperation,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    loaders::fetch_supported_versions(operation, COMPONENT_ID).await
}

/// The retained provider preserves Maven build ordering, exact prerelease
/// coordinates, recommended/latest promotions, and optional-promotion failure.
pub async fn fetch_builds(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    let (records, state) =
        loaders::fetch_builds(operation, COMPONENT_ID, minecraft_version).await?;
    for record in &records {
        validate_record(record)?;
    }
    Ok((records, state))
}

pub fn fetch_cached_builds(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<Option<(Vec<LoaderBuildRecord>, LoaderCatalogState)>, LoaderError> {
    let result = loaders::fetch_cached_builds(operation, COMPONENT_ID, minecraft_version)?;
    if let Some((records, _)) = &result {
        for record in records {
            validate_record(record)?;
        }
    }
    Ok(result)
}

/// Resolve opaque build identity through the live catalog, preserving provider
/// errors and stale/unavailable/build-removed distinctions.
pub async fn resolve(build_id: &str) -> Result<PreparedForgeBuild, LoaderError> {
    match loaders::parse_build_id(build_id) {
        Some((LoaderComponentId::Forge, _, _)) => {}
        _ => return Err(LoaderError::InvalidBuildId),
    }
    PreparedForgeBuild::prepare(
        loaders::resolve_build_record_for_install(LoaderComponentId::Forge, build_id).await?,
    )
}

pub async fn resolve_build(build_id: &str) -> Result<LoaderBuildRecord, LoaderError> {
    resolve(build_id).await.map(PreparedForgeBuild::into_record)
}

/// Shared queue entrypoint with exact source validation before effects. The
/// returned receipt/error is retained unchanged; only premature terminal progress
/// callbacks are withheld until the queue settles activation and publication.
pub async fn install_build<F>(
    operation: &ManagedLibraryOperation,
    runtime_cache: ManagedRuntimeCache,
    record: LoaderBuildRecord,
    send: F,
) -> Result<LoaderInstallPublicationOutcome, LoaderInstallError>
where
    F: FnMut(DownloadProgress),
{
    PreparedForgeBuild::prepare(record)
        .map_err(LoaderInstallError::from)?
        .install_base(operation, runtime_cache, send)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::loaders::providers::{
        apply_forge_promotion_selection, infer_loader_build_metadata,
    };
    use axial_minecraft::loaders::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildSubjectKind, LoaderInstallSource,
        LoaderInstallStrategy, LoaderInstallability, LoaderSelectionReason, LoaderSelectionSource,
        LoaderVersionIndex, persist_loader_build_cache_fixture_for_test,
    };
    use axial_minecraft::managed_path::ManagedLibraryTestAuthority;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn record(minecraft: &str, forge: &str) -> LoaderBuildRecord {
        let component_id = LoaderComponentId::Forge;
        let (strategy, artifact_kind, install_source) =
            forge_install_source(minecraft, forge).unwrap();
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: loaders::build_id_for(component_id, minecraft, forge),
            minecraft_version: minecraft.to_string(),
            loader_version: forge.to_string(),
            version_id: loaders::installed_version_id_for(component_id, minecraft, forge).unwrap(),
            build_meta: LoaderBuildMetadata::default(),
            strategy,
            artifact_kind,
            installability: LoaderInstallability::Installable,
            install_source,
        }
    }

    #[test]
    fn admits_every_retained_forge_era_with_exact_artifact_coordinates() {
        use LoaderInstallStrategy::{ForgeEarliestLegacy, ForgeLegacyInstaller, ForgeModern};
        let cases = [
            ("1.1", "1.3.4.29", ForgeEarliestLegacy, "client.zip"),
            ("1.2.4", "2.0.0.68", ForgeEarliestLegacy, "client.zip"),
            ("1.3.2", "4.3.5.318", ForgeEarliestLegacy, "universal.zip"),
            ("1.4.7", "6.6.2.534", ForgeEarliestLegacy, "universal.zip"),
            ("1.5.2", "7.8.1.738", ForgeLegacyInstaller, "installer.jar"),
            (
                "1.6.4",
                "9.11.1.1345",
                ForgeLegacyInstaller,
                "installer.jar",
            ),
            (
                "1.7.10_pre4",
                "10.12.2.1149-prerelease",
                ForgeLegacyInstaller,
                "installer.jar",
            ),
            (
                "1.12.2",
                "14.23.5.2859",
                ForgeLegacyInstaller,
                "installer.jar",
            ),
            ("1.13", "25.0.0", ForgeModern, "installer.jar"),
            ("1.21.11", "61.1.5", ForgeModern, "installer.jar"),
            ("26.2-rc-1", "65.0.0", ForgeModern, "installer.jar"),
        ];
        for (minecraft, forge, strategy, suffix) in cases {
            let prepared = PreparedForgeBuild::prepare(record(minecraft, forge)).unwrap();
            assert_eq!(prepared.record().strategy, strategy);
            let source = match &prepared.record().install_source {
                LoaderInstallSource::LegacyArchive { url }
                | LoaderInstallSource::InstallerJar { url } => url,
                other => panic!("unexpected source {other:?}"),
            };
            assert_eq!(
                source,
                &format!(
                    "https://maven.minecraftforge.net/net/minecraftforge/forge/{minecraft}-{forge}/forge-{minecraft}-{forge}-{suffix}"
                )
            );
            assert_eq!(
                loaders::parse_build_id(&prepared.record().build_id),
                Some((
                    LoaderComponentId::Forge,
                    minecraft.to_string(),
                    forge.to_string(),
                ))
            );
        }
    }

    #[test]
    fn rejects_swapped_build_child_and_provider_authority() {
        let valid = record("1.20.1", "47.4.0");
        let mut changed = valid.clone();
        changed.component_id = LoaderComponentId::NeoForge;
        assert!(PreparedForgeBuild::prepare(changed).is_err());

        let mut changed = valid.clone();
        changed.build_id = loaders::build_id_for(LoaderComponentId::Forge, "1.20.1", "47.3.0");
        assert!(PreparedForgeBuild::prepare(changed).is_err());

        let mut changed = valid.clone();
        changed.version_id = "1.20.1".to_string();
        assert!(PreparedForgeBuild::prepare(changed).is_err());

        let mut changed = valid.clone();
        changed.strategy = LoaderInstallStrategy::ForgeEarliestLegacy;
        assert!(PreparedForgeBuild::prepare(changed).is_err());

        let mut changed = valid.clone();
        changed.artifact_kind = LoaderArtifactKind::LegacyArchive;
        assert!(PreparedForgeBuild::prepare(changed).is_err());

        let mut changed = valid;
        changed.install_source = LoaderInstallSource::InstallerJar {
            url: "http://127.0.0.1/untrusted-installer.jar".to_string(),
        };
        assert!(PreparedForgeBuild::prepare(changed).is_err());
    }

    #[test]
    fn child_and_base_identities_remain_distinct_across_strategies() {
        for (minecraft, forge) in [
            ("1.2.4", "2.0.0.68"),
            ("1.6.4", "9.11.1.1345"),
            ("1.21.11", "61.1.5"),
        ] {
            let prepared = PreparedForgeBuild::prepare(record(minecraft, forge)).unwrap();
            let record = prepared.into_record();
            assert_ne!(record.version_id, minecraft);
            assert!(
                loaders::validate_materialized_loader_profile(
                    &record.version_id,
                    &record.version_id,
                    minecraft,
                    true,
                )
                .is_ok()
            );
            assert!(
                loaders::validate_materialized_loader_profile(
                    &record.version_id,
                    &record.version_id,
                    "wrong-base",
                    true,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn promotions_keep_recommended_latest_and_unstable_selection_distinct() {
        let cases = [
            (
                "47.4.0",
                true,
                true,
                false,
                1000,
                LoaderSelectionReason::Recommended,
            ),
            (
                "47.4.1",
                true,
                false,
                true,
                900,
                LoaderSelectionReason::LatestStable,
            ),
            (
                "47.3.0",
                true,
                false,
                false,
                800,
                LoaderSelectionReason::Stable,
            ),
            (
                "47.4.1-beta",
                true,
                true,
                true,
                650,
                LoaderSelectionReason::LatestUnstable,
            ),
            (
                "47.4.1-beta",
                true,
                true,
                false,
                600,
                LoaderSelectionReason::Unstable,
            ),
        ];
        for (version, has_recommended, recommended, latest, rank, reason) in cases {
            let mut metadata = infer_loader_build_metadata(version, &[], recommended, latest, None);
            apply_forge_promotion_selection(&mut metadata, has_recommended, recommended, latest);
            assert_eq!(metadata.selection.default_rank, rank);
            assert_eq!(metadata.selection.reason, reason);
        }
        let mut no_promotions = infer_loader_build_metadata("47.4.0", &[], false, false, None);
        apply_forge_promotion_selection(&mut no_promotions, false, false, false);
        assert_eq!(no_promotions.selection.default_rank, 800);
        assert_eq!(no_promotions.selection.source, LoaderSelectionSource::None);
    }

    #[tokio::test]
    async fn invalid_or_other_component_selection_fails_before_provider_io() {
        for id in [
            "forge:1.21.11:61.1.5".to_string(),
            loaders::build_id_for(LoaderComponentId::NeoForge, "1.21.1", "21.1.0"),
        ] {
            assert!(matches!(
                resolve(&id).await,
                Err(LoaderError::InvalidBuildId)
            ));
        }
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
        // Catalog queries retain the leaf's trim normalization. Opaque build
        // identities above remain exact and reject cross-component selections.
        let expected = record("1.21.1", "52.0.0");
        persist_loader_build_cache_fixture_for_test(
            authority.operation(),
            "1.21.1",
            &LoaderVersionIndex {
                component_id: COMPONENT_ID,
                builds: vec![expected.clone()],
            },
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
        )
        .unwrap();
        let (builds, state) = fetch_builds(authority.operation(), " 1.21.1")
            .await
            .unwrap();
        assert_eq!(builds, vec![expected]);
        assert!(state.availability.cache_hit);
    }

    #[tokio::test]
    async fn fresh_historical_build_cache_preserves_exact_promoted_records() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
        for (minecraft, forge) in [
            ("1.2.4", "2.0.0.68"),
            ("1.6.4", "9.11.1.1345"),
            ("1.21.11", "61.1.5"),
        ] {
            let mut expected = record(minecraft, forge);
            expected.build_meta = infer_loader_build_metadata(forge, &[], true, false, None);
            apply_forge_promotion_selection(&mut expected.build_meta, true, true, false);
            persist_loader_build_cache_fixture_for_test(
                authority.operation(),
                minecraft,
                &LoaderVersionIndex {
                    component_id: COMPONENT_ID,
                    builds: vec![expected.clone()],
                },
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64,
            )
            .unwrap();

            let (builds, state) = fetch_builds(authority.operation(), minecraft)
                .await
                .unwrap();
            assert_eq!(builds, vec![expected]);
            assert!(state.availability.cache_hit);
            assert!(state.availability.fresh);
            assert!(!state.availability.stale);
        }
    }

    #[tokio::test]
    async fn invalid_install_source_has_no_files_or_progress_effects() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
        let runtime = ManagedRuntimeCache::isolated_for_test().unwrap();
        let mut invalid = record("1.20.1", "47.4.0");
        invalid.install_source = LoaderInstallSource::InstallerJar {
            url: "http://127.0.0.1/untrusted-installer.jar".to_string(),
        };
        let before = std::fs::read_dir(root.path()).unwrap().count();
        let mut events = Vec::new();
        let outcome = install_build(authority.operation(), runtime, invalid, |event| {
            events.push(event)
        })
        .await;
        assert!(outcome.is_err());
        assert!(events.is_empty());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
    }

    #[test]
    fn leaf_terminal_progress_cannot_publish_queue_completion() {
        let pending = DownloadProgress {
            phase: "processors".to_string(),
            current: 1,
            total: 2,
            file: None,
            error: None,
            done: false,
            bytes_done: None,
            bytes_total: None,
        };
        let mut events = Vec::new();
        for event in [
            pending.clone(),
            DownloadProgress {
                done: true,
                ..pending.clone()
            },
            DownloadProgress {
                error: Some("private processor diagnostic".to_string()),
                done: true,
                ..pending.clone()
            },
        ] {
            forward_pending_progress(event, &mut |event| events.push(event));
        }
        assert_eq!(events, vec![pending]);
    }
}
