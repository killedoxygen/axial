use super::*;
use crate::{
    accounts::{
        credential_store::CredentialStore, directory::AccountDirectory, session::AuthService,
    },
    launch::{
        coordinator::{LaunchCoordinator, LaunchError},
        session::SessionManager,
    },
    network::{ClientConfig, ProviderClient},
    performance::PerformanceService,
    runtime::discovery::RuntimeDiscovery,
    skins::{ProfileMedia, library::SavedSkinLibrary, store::SavedSkinStore},
};

pub(super) struct LoaderCatalogFixture {
    pub builds: Vec<LoaderBuildRecord>,
    pub versions: Vec<axial_minecraft::LoaderGameVersion>,
    pub state: loaders::LoaderCatalogState,
}

fn fixture() -> (tempfile::TempDir, SetupService, Arc<AccountDirectory>) {
    let root = tempfile::Builder::new()
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap();
    let library = match crate::library::LibraryLifecycle::open(root.path()) {
        crate::library::LibraryOpenOutcome::Ready(library) => library,
        other => panic!("isolated test library did not open: {other:?}"),
    };
    let storage = Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap());
    storage
        .migrate(&[
            super::super::directory::MIGRATION,
            super::super::create::MIGRATION,
            super::super::create::DUPLICATE_WITNESS_MIGRATION,
            super::super::import::MIGRATION,
            super::super::delete::MIGRATION,
            crate::content::install::MIGRATION,
            crate::performance::mutation::MIGRATION,
            crate::accounts::directory::MIGRATION,
            crate::install::queue::MIGRATION,
            crate::install::queue::MIGRATION_V2,
            crate::performance::rules::MIGRATION,
            crate::skins::store::MIGRATION,
        ])
        .unwrap();
    let instances = Arc::new(InstanceService::new(
        super::super::directory::InstanceDirectories::new(
            super::super::directory::Registry::new(storage.clone()),
            library,
            crate::tasks::Exclusions::new(),
        ),
        crate::tasks::TaskOwner::new(16).unwrap(),
    ));
    let tasks = instances.tasks.clone();
    let directories = instances.directories().clone();
    let library = directories.library();
    let runtime = axial_minecraft::ManagedRuntimeCache::isolated_for_test().unwrap();
    let queue = Arc::new(
        InstallQueue::new(
            storage.clone(),
            library.clone(),
            directories.exclusions().clone(),
            tasks.clone(),
            runtime.clone(),
        )
        .unwrap(),
    );
    let client = ProviderClient::new(ClientConfig::default()).unwrap();
    let content = Arc::new(ContentService::new(client.clone()).unwrap());
    let settings = Arc::new(SettingsStore::new(storage.clone()).unwrap());
    settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": 0, "performance_mode": "vanilla",
                "java_path_override": root.path().join("no-java-here").to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    let accounts = Arc::new(AccountDirectory::new(storage.clone()).unwrap());
    let auth = Arc::new(AuthService::new(
        accounts.clone(),
        Arc::new(CredentialStore::isolated_for_tests()),
        tasks.clone(),
    ));
    let performance = PerformanceService::new(
        storage.clone(),
        directories.clone(),
        tasks.clone(),
        content,
        crate::performance::public_transfer_resolver(),
    )
    .unwrap();
    let root_pin = library.admit_application_root().unwrap();
    let skins = ProfileMedia::new(
        Arc::new(SavedSkinLibrary::new(
            SavedSkinStore::new(storage),
            root_pin.clone(),
        )),
        accounts.clone(),
        auth.clone(),
        tasks.clone(),
        root_pin,
    )
    .unwrap();
    let launch = Arc::new(LaunchCoordinator::new(
        directories,
        accounts.clone(),
        settings.clone(),
        queue.as_ref().clone(),
        RuntimeDiscovery::new(runtime, tasks.clone()),
        performance,
        auth,
        skins,
        SessionManager::new(tasks.clone()),
        tasks,
    ));
    let service = SetupService::new(
        instances,
        Arc::new(Catalog::new(client)),
        queue,
        settings,
        launch,
    );
    seed_catalog(&service);
    (root, service, accounts)
}

fn seed_catalog(service: &SetupService) {
    let pin = service.instances.directories().library().admit().unwrap();
    let operation = pin.managed_library().unwrap();
    let versions = ["1.21.4", "1.20.1"]
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "id":id, "type":"release", "url":format!("https://piston-meta.mojang.com/{id}.json"),
                "sha1":"a".repeat(40), "time":"2024-12-03T00:00:00Z", "releaseTime":"2024-12-03T00:00:00Z"
            })
        })
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "latest": {"release":"1.21.4", "snapshot":"1.21.4"},
        "versions": versions
    });
    axial_minecraft::manifest::persist_version_manifest_cache_fixture_for_test(
        &operation,
        &serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

fn profile_build(
    component: LoaderComponentId,
    minecraft: &str,
    loader: &str,
    reason: LoaderSelectionReason,
) -> LoaderBuildRecord {
    let (strategy, base) = match component {
        LoaderComponentId::Fabric => ("fabric_profile", "https://meta.fabricmc.net/v2/versions"),
        LoaderComponentId::Quilt => ("quilt_profile", "https://meta.quiltmc.org/v3/versions"),
        _ => panic!("profile fixture requires Fabric or Quilt"),
    };
    serde_json::from_value(serde_json::json!({
        "component_id":component, "component_name":component.display_name(),
        "build_id":loaders::build_id_for(component, minecraft, loader),
        "minecraft_version":minecraft, "loader_version":loader,
        "version_id":loaders::installed_version_id_for(component, minecraft, loader).unwrap(),
        "build_meta":{"selection":{"reason":reason}},
        "strategy":strategy, "artifact_kind":"profile_json", "installability":"installable",
        "install_source":{"kind":"profile_json", "url":format!("{base}/loader/{minecraft}/{loader}/profile/json")}
    }))
    .unwrap()
}

fn stale_loader_catalog() -> LoaderCatalogFixture {
    LoaderCatalogFixture {
        builds: vec![profile_build(
            LoaderComponentId::Fabric,
            "1.21.4",
            "0.16.14",
            LoaderSelectionReason::Stable,
        )],
        versions: vec![serde_json::from_value(serde_json::json!({"id":"1.21.4"})).unwrap()],
        state: loaders::LoaderCatalogState {
            availability: loaders::LoaderAvailability {
                fresh: false,
                stale: true,
                cache_hit: true,
                ..Default::default()
            },
        },
    }
}

#[tokio::test]
async fn loader_picker_retains_beta_labels_without_treating_unknown_as_an_unstable_default() {
    let (_root, mut service, _) = fixture();
    let cases = [
        (LoaderSelectionReason::Recommended, false),
        (LoaderSelectionReason::LatestStable, false),
        (LoaderSelectionReason::Stable, false),
        (LoaderSelectionReason::Unlabeled, false),
        (LoaderSelectionReason::Latest, true),
        (LoaderSelectionReason::LatestUnstable, true),
        (LoaderSelectionReason::Unstable, true),
        (LoaderSelectionReason::Unknown, false),
    ];
    let mut catalog = stale_loader_catalog();
    catalog.state.availability.fresh = true;
    catalog.state.availability.stale = false;
    catalog.builds = cases
        .iter()
        .enumerate()
        .map(|(index, (reason, _))| {
            profile_build(
                LoaderComponentId::Fabric,
                "1.21.4",
                &format!("0.16.{}", index + 14),
                *reason,
            )
        })
        .collect();
    let unknown = catalog.builds.last().unwrap().clone();
    service.loader_catalog_fixture = Some(catalog);
    let view = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(view.auto.enabled);
    for (index, (option, (_, beta))) in view.builds.iter().zip(cases).enumerate() {
        assert_eq!(option.channel_id, if beta { "beta" } else { "stable" });
        assert_eq!(option.channel_label, if beta { "Beta" } else { "Stable" });
        assert_eq!(option.recommended, index == 0);
        assert!(option.enabled);
    }
    service.loader_catalog_fixture.as_mut().unwrap().builds = vec![unknown];
    let unknown_only = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!unknown_only.auto.enabled);
    assert!(!unknown_only.builds[0].recommended);
    assert_eq!(unknown_only.builds[0].channel_id, "stable");
    assert!(unknown_only.builds[0].enabled);
}

#[tokio::test]
async fn incompatible_quilt_build_is_disabled_and_rejected_even_when_installed() {
    let (_root, mut service, _) = fixture();
    let component = LoaderComponentId::Quilt;
    let minecraft = "26.1.2";
    let incompatible = profile_build(
        component,
        minecraft,
        "0.29.2",
        LoaderSelectionReason::Unlabeled,
    );
    let compatible = profile_build(
        component,
        minecraft,
        "0.30.0-beta.8",
        LoaderSelectionReason::Unstable,
    );
    let mut catalog = stale_loader_catalog();
    catalog.builds = vec![incompatible.clone(), compatible.clone()];
    catalog.state.availability.fresh = true;
    catalog.state.availability.stale = false;
    service.loader_catalog_fixture = Some(catalog);
    for version in [minecraft, &incompatible.version_id, &compatible.version_id] {
        crate::install::queue::tests::install_ready_fixture(&service.installs, version).await;
    }
    let pin = service.instances.directories().library().admit().unwrap();
    assert!(
        service
            .installs
            .ready_version(&pin, &incompatible.version_id)
            .await
            .is_ok(),
        "the compatibility gate must not rely on missing or damaged artifacts"
    );
    for fresh in [true, false] {
        let availability = &mut service
            .loader_catalog_fixture
            .as_mut()
            .unwrap()
            .state
            .availability;
        availability.fresh = fresh;
        availability.stale = !fresh;
        let view = service
            .loader_builds(component.as_str(), minecraft)
            .await
            .unwrap();
        assert!(view.auto.enabled);
        assert!(view.builds[0].installed && !view.builds[0].enabled);
        assert!(!view.builds[0].recommended);
        assert_eq!(
            view.builds[0].disabled_reason.as_deref(),
            Some("This Quilt build is known to be incompatible with Minecraft 26.1.2.")
        );
        assert!(view.builds[1].installed && view.builds[1].enabled && view.builds[1].recommended);
        assert_eq!(view.builds[1].channel_label, "Beta");
    }
    let automatic = format!("loader_auto|{}|{minecraft}", component.as_str());
    assert_eq!(
        service.resolve(&automatic).await.unwrap().0.version_id(),
        compatible.version_id
    );
    let exact_compatible = format!(
        "loader_build|{}|{}",
        component.as_str(),
        compatible.build_id
    );
    assert_eq!(
        service
            .resolve(&exact_compatible)
            .await
            .unwrap()
            .0
            .version_id(),
        compatible.version_id
    );
    service.loader_catalog_fixture.as_mut().unwrap().builds = vec![incompatible.clone()];
    let unavailable = service
        .loader_builds(component.as_str(), minecraft)
        .await
        .unwrap();
    assert!(!unavailable.auto.enabled && !unavailable.builds[0].enabled);
    assert!(matches!(
        service.resolve(&automatic).await,
        Err(InstanceError::VersionUnavailable)
    ));
    let before = service.installs.snapshot();
    let service = Arc::new(service);
    let result = service
        .create(CreateInstanceRequest {
            name: "Incompatible installed Quilt".into(),
            selection_id: format!(
                "loader_build|{}|{}",
                component.as_str(),
                incompatible.build_id
            ),
            ..Default::default()
        })
        .await;
    assert!(matches!(result, Err(InstanceError::VersionUnavailable)));
    assert!(service.instances.registry().list().unwrap().is_empty());
    assert_eq!(service.installs.snapshot(), before);
}

#[tokio::test]
async fn instance_enrichment_leaves_resource_counts_to_detailed_reads_even_during_launch() {
    let (_root, service, _) = fixture();
    let instance = super::super::create::tests::create(&service.instances, "Resource counts").await;
    let game = super::super::create::tests::payload_path(&service.instances, &instance.id);
    std::fs::write(game.join("mods/.axial-lock.json"), b"internal metadata").unwrap();
    std::fs::create_dir(game.join("mods/.axial-performance")).unwrap();
    std::fs::write(game.join("mods/active.jar"), b"mod").unwrap();
    std::fs::write(game.join("mods/inactive.jar.disabled"), b"disabled mod").unwrap();
    std::fs::create_dir(game.join("saves/World")).unwrap();
    std::fs::write(game.join("resourcepacks/pack.zip"), b"resources").unwrap();
    std::fs::write(game.join("shaderpacks/pack.zip"), b"shaders").unwrap();
    for during_launch in [false, true] {
        let launch =
            during_launch.then(|| service.instances.directories().admit(&instance.id).unwrap());
        let enriched = service.enrich(instance.clone(), &[]).await;
        assert_eq!(
            [
                enriched.saves_count,
                enriched.mods_count,
                enriched.resource_count,
                enriched.shader_count,
            ],
            [0; 4]
        );
        assert!(!enriched.counts_available);
        let read = service
            .instances
            .directories()
            .admit_read(&instance.id)
            .unwrap();
        let mods = crate::resources::mods::list_mods(read.game_directory()).unwrap();
        read.validate_current().unwrap();
        assert_eq!(mods.len(), 2);
        assert_eq!(mods.iter().filter(|entry| entry.enabled).count(), 1);
        drop(launch);
    }
}

#[tokio::test]
async fn second_instance_reuses_exact_verified_install_without_requeueing() {
    let (_root, service, _) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let before = service.installs.snapshot();
    let service = Arc::new(service);
    let first = service
        .create(super::super::create::tests::request("First ready instance"))
        .await
        .unwrap();
    let second = service
        .create(super::super::create::tests::request(
            "Second ready instance",
        ))
        .await
        .unwrap();
    assert_ne!(first.instance.instance.id, second.instance.instance.id);
    assert!(first.install_queue.is_none());
    assert!(second.install_queue.is_none());
    assert_eq!(second.view_model.summary, "Instance created.");
    assert_eq!(service.installs.snapshot(), before);
    assert_eq!(service.instances.registry().list().unwrap().len(), 2);
}

#[tokio::test]
async fn scanner_ready_artifact_drift_offers_install_without_masking_transient_blocks() {
    let (root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    let account_block = service.enrich(instance.clone(), &versions).await;
    assert_eq!(account_block.launch_action.primary_action, "blocked");
    assert!(account_block.status_detail.contains("account"));
    accounts.create_offline_account("FixturePlayer").unwrap();
    let pin = service.instances.directories().library().admit().unwrap();
    let operation = pin.managed_library().unwrap();
    let publication =
        axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).unwrap();
    let busy = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(busy.error.unwrap().code, LaunchError::InstanceBusy);
    let blocked = service.enrich(instance.clone(), &versions).await;
    assert_eq!(blocked.launch_action.primary_action, "blocked");
    assert_eq!(blocked.launch_action.label, "Unavailable");
    assert!(blocked.needs_install.is_empty());
    drop(publication);
    let runtime_block = service.enrich(instance.clone(), &versions).await;
    assert_eq!(runtime_block.launch_action.primary_action, "blocked");
    assert!(runtime_block.status_detail.contains("Java runtime"));
    std::fs::write(
        root.path().join("versions/1.21.4/1.21.4.jar"),
        b"externally changed",
    )
    .unwrap();
    let scanned = service.installed().await.unwrap();
    assert!(
        scanned
            .iter()
            .any(|entry| entry.id == "1.21.4" && entry.launchable)
    );
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        preflight.error.unwrap().code,
        LaunchError::InstallUnavailable
    );
    let drifted = service.enrich(instance, &scanned).await;
    assert_eq!(drifted.launch_action.primary_action, "install");
    assert_eq!(drifted.launch_action.label, "Install");
    assert_eq!(drifted.needs_install, "1.21.4");
    assert!(!drifted.launchable);
    assert!(drifted.install_target.is_some());
}

fn unready_catalogs(versions: &[VersionEntry]) -> [Vec<VersionEntry>; 3] {
    let mut not_installed = versions.to_vec();
    for version in &mut not_installed {
        version.installed = false;
    }
    let mut not_launchable = versions.to_vec();
    for version in &mut not_launchable {
        version.launchable = false;
    }
    [Vec::new(), not_installed, not_launchable]
}

#[tokio::test]
async fn absent_or_unready_catalog_does_not_turn_launch_blocks_into_install_permission() {
    let (_root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    assert!(
        versions
            .iter()
            .any(|version| version.id == instance.version_id)
    );

    let no_account = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        no_account.error.unwrap().code,
        LaunchError::AccountUnavailable
    );
    for catalog in unready_catalogs(&versions) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.status_detail.contains("account"));
        assert!(blocked.needs_install.is_empty());
        assert!(blocked.install_target.is_some());
    }

    accounts.create_offline_account("FixturePlayer").unwrap();
    let pin = service.instances.directories().library().admit().unwrap();
    let operation = pin.managed_library().unwrap();
    let publication =
        axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).unwrap();
    let busy = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(busy.error.unwrap().code, LaunchError::InstanceBusy);
    for catalog in unready_catalogs(&versions) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert_eq!(blocked.status_detail, LaunchError::InstanceBusy.to_string());
        assert!(blocked.needs_install.is_empty());
    }
    drop(publication);

    let runtime = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(runtime.error.unwrap().code, LaunchError::RuntimeUnavailable);
    for catalog in unready_catalogs(&versions) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(
            blocked.status_detail,
            LaunchError::RuntimeUnavailable.to_string()
        );
        assert!(blocked.needs_install.is_empty());
    }
}

#[tokio::test]
async fn guarded_missing_install_evidence_offers_install_without_a_selected_account() {
    for drifted in [false, true] {
        let (root, service, _accounts) = fixture();
        let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        let versions = service.installed().await.unwrap();
        if drifted {
            std::fs::write(
                root.path().join("versions/1.21.4/1.21.4.jar"),
                b"externally changed",
            )
            .unwrap();
        } else {
            service
                .instances
                .registry()
                .storage()
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "DELETE FROM installed_versions WHERE version_id = '1.21.4'",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let preflight = service.launch.preflight(instance.id.clone()).await;
        assert_eq!(
            preflight.error.unwrap().code,
            LaunchError::InstallUnavailable
        );
        for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
            let missing = service.enrich(instance.clone(), &catalog).await;
            assert_eq!(missing.launch_action.primary_action, "install");
            assert_eq!(missing.launch_action.label, "Install");
            assert_eq!(missing.needs_install, instance.version_id);
            assert!(!missing.launchable);
            assert_eq!(
                missing.install_target.unwrap().version_id,
                instance.version_id
            );
        }
    }
}

#[tokio::test]
async fn unavailable_install_metadata_does_not_offer_reinstall() {
    let (_root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    accounts.create_offline_account("FixturePlayer").unwrap();
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TABLE installed_versions")?;
            Ok(())
        })
        .unwrap();
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        preflight.error.unwrap().code,
        LaunchError::LibraryUnavailable
    );
    for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.needs_install.is_empty());
    }
}

#[tokio::test]
async fn unavailable_setup_metadata_does_not_fabricate_resume_or_install_permission() {
    let (_root, service, _accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DELETE FROM installed_versions")?;
            Ok(())
        })
        .unwrap();
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        preflight.error.unwrap().code,
        LaunchError::InstallUnavailable
    );
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TABLE instance_setups")?;
            Ok(())
        })
        .unwrap();
    assert!(has_pending(service.instances.registry().storage(), &instance.id).is_err());
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert!(!preflight.launchable);
    assert_eq!(preflight.error.unwrap().code, LaunchError::InstanceChanged);
    for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert!(!blocked.launchable);
        assert!(!blocked.launch_action.launchable);
        assert_eq!(blocked.launch_action.state_id, "blocked");
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.needs_install.is_empty());
        assert_eq!(
            blocked.status_detail,
            "Instance setup status could not be read. Refresh and try again."
        );
        assert_eq!(
            blocked.launch_action.disabled_reason.as_deref(),
            Some(blocked.status_detail.as_str())
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_setup_metadata_blocks_an_otherwise_launchable_instance() {
    use std::os::unix::fs::PermissionsExt;

    let (root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    accounts.create_offline_account("FixturePlayer").unwrap();
    let java = root.path().join("java");
    std::fs::write(
        &java,
        format!(
            "#!/bin/sh\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": service.settings.current().unwrap().revision,
                "java_path_override": java.to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    let ready = service.enrich(instance.clone(), &versions).await;
    assert!(ready.launchable, "{}", ready.status_detail);
    assert_eq!(ready.launch_action.primary_action, "launch");
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TABLE instance_setups")?;
            Ok(())
        })
        .unwrap();
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert!(!preflight.launchable);
    assert_eq!(preflight.error.unwrap().code, LaunchError::InstanceChanged);
    for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert!(!blocked.launchable);
        assert!(!blocked.launch_action.launchable);
        assert_eq!(blocked.launch_action.state_id, "blocked");
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.needs_install.is_empty());
        assert_eq!(
            blocked.status_detail,
            "Instance setup status could not be read. Refresh and try again."
        );
        assert_eq!(
            blocked.launch_action.disabled_reason.as_deref(),
            Some(blocked.status_detail.as_str())
        );
    }
}

#[tokio::test]
async fn create_view_download_indicators_use_settled_install_and_scanner_status() {
    let (root, mut service, _) = fixture();
    service.loader_catalog_fixture = Some(stale_loader_catalog());
    let initial = service.create_view(None).await.unwrap();
    assert!(
        initial
            .versions
            .iter()
            .all(|row| row.download_state == "none")
    );
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let installed = service.create_view(None).await.unwrap();
    assert_eq!(
        installed
            .versions
            .iter()
            .find(|row| row.minecraft_version_id == "1.21.4")
            .unwrap()
            .download_state,
        "full"
    );
    let loader = service
        .create_view(Some(LoaderComponentId::Fabric.as_str()))
        .await
        .unwrap();
    assert_eq!(loader.versions[0].download_state, "base");
    assert!(
        !loader.versions[0].create_enabled,
        "the base does not authorize a stale loader choice"
    );
    std::fs::write(root.path().join("versions/1.21.4/1.21.4.jar"), b"changed").unwrap();
    let drifted = service.create_view(None).await.unwrap();
    assert_eq!(
        drifted
            .versions
            .iter()
            .find(|row| row.minecraft_version_id == "1.21.4")
            .unwrap()
            .download_state,
        "full",
        "displayed installation is not a full-payload integrity attestation"
    );
    std::fs::remove_file(root.path().join("versions/1.21.4/1.21.4.jar")).unwrap();
    let incomplete = service.create_view(None).await.unwrap();
    assert_eq!(
        incomplete
            .versions
            .iter()
            .find(|row| row.minecraft_version_id == "1.21.4")
            .unwrap()
            .download_state,
        "none",
        "a settled metadata row alone must not override an incomplete scan"
    );
}

#[tokio::test]
async fn offline_loader_creation_reuses_only_the_exact_preferred_verified_build() {
    let (root, mut service, _) = fixture();
    let catalog = stale_loader_catalog();
    let installed = catalog.builds[0].clone();
    service.loader_catalog_fixture = Some(catalog);
    let selection = "loader_auto|net.fabricmc.fabric-loader|1.21.4";
    assert!(matches!(
        service.resolve(selection).await,
        Err(InstanceError::VersionUnavailable)
    ));
    let absent = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!absent.auto.enabled);
    assert!(absent.auto.disabled_reason.is_some());
    assert!(!absent.builds[0].enabled);
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    crate::install::queue::tests::install_ready_fixture(&service.installs, &installed.version_id)
        .await;
    let scanned = service.installed().await.unwrap();
    assert!(scanned.iter().any(|entry| {
        entry.id == installed.version_id
            && entry.launchable
            && entry.loader.as_ref().is_some_and(|loader| {
                loader.component_id == installed.component_id
                    && loader.build_id == installed.build_id
            })
    }));
    let loader_rows = service
        .create_view(Some(LoaderComponentId::Fabric.as_str()))
        .await
        .unwrap();
    assert_eq!(loader_rows.versions[0].download_state, "full");
    assert!(loader_rows.versions[0].create_enabled);
    let available = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(available.auto.enabled);
    assert!(available.auto.disabled_reason.is_none());
    assert!(available.builds[0].enabled && available.builds[0].installed);
    let before = service.installs.snapshot();
    let (target, request) = service.resolve(selection).await.unwrap();
    assert_eq!(target.version_id(), installed.version_id);
    assert_eq!(
        request,
        InstallQueueRequest::Loader {
            component_id: installed.component_id,
            build_id: installed.build_id.clone()
        }
    );
    let exact = format!(
        "loader_build|{}|{}",
        installed.component_id.as_str(),
        installed.build_id
    );
    assert_eq!(
        service.resolve(&exact).await.unwrap().0.version_id(),
        installed.version_id
    );
    let mut newer = installed.clone();
    newer.loader_version = "0.16.15".into();
    newer.build_id = loaders::build_id_for(newer.component_id, "1.21.4", "0.16.15");
    newer.version_id =
        loaders::installed_version_id_for(newer.component_id, "1.21.4", "0.16.15").unwrap();
    service.loader_catalog_fixture.as_mut().unwrap().builds = vec![newer, installed.clone()];
    assert!(
        matches!(
            service.resolve(selection).await,
            Err(InstanceError::VersionUnavailable)
        ),
        "automatic selection must not substitute an older installed build for the preferred build"
    );
    let choices = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!choices.auto.enabled);
    assert!(choices.auto.disabled_reason.is_some());
    assert!(choices.builds[0].recommended && !choices.builds[0].enabled);
    assert!(choices.builds[1].installed && choices.builds[1].enabled);
    assert_eq!(
        service.resolve(&exact).await.unwrap().0.version_id(),
        installed.version_id,
        "the older explicit installed choice remains usable"
    );
    service
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .state
        .availability
        .fresh = true;
    service
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .state
        .availability
        .stale = false;
    let fresh = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(fresh.auto.enabled && fresh.builds[0].enabled);
    service
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .builds
        .clear();
    let empty = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!empty.auto.enabled && empty.auto.disabled_reason.is_some());
    service.loader_catalog_fixture = Some(stale_loader_catalog());
    let service = Arc::new(service);
    let created = service
        .create(CreateInstanceRequest {
            name: "Offline Fabric".into(),
            selection_id: selection.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.instance.instance.version_id, installed.version_id);
    assert!(created.install_queue.is_none());
    assert_eq!(service.installs.snapshot(), before);
    std::fs::write(
        root.path()
            .join(format!("versions/{0}/{0}.jar", installed.version_id)),
        b"changed",
    )
    .unwrap();
    assert!(matches!(
        service.resolve(selection).await,
        Err(InstanceError::VersionUnavailable)
    ));
}

#[tokio::test]
async fn first_instance_still_queues_uninstalled_runtime() {
    let (_root, service, _) = fixture();
    let pin = service.instances.directories().library().admit().unwrap();
    let blocker = service
        .instances
        .directories()
        .exclusions()
        .try_acquire_read_artifacts(
            std::iter::empty::<String>(),
            [crate::install::queue::library_artifact(
                &pin.library_id().to_string(),
            )],
        )
        .unwrap();
    let service = Arc::new(service);
    let created = service
        .create(super::super::create::tests::request("First installation"))
        .await
        .unwrap();
    let queue = created.install_queue.unwrap();
    assert!(queue.active.is_none());
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].install_item.version_id, "1.21.4");
    assert_eq!(created.instance.launch_action.primary_action, "install");
    service.installs.shutdown_queued().unwrap();
    drop(blocker);
}
