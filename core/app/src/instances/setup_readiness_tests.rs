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
    let (service, accounts) = open_fixture(root.path(), crate::library::LibraryId::new());
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": 0, "performance_mode": "vanilla",
                "java_path_override": root.path().join("no-java-here").to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    seed_catalog(&service);
    (root, service, accounts)
}

fn open_fixture(
    root: &std::path::Path,
    library_id: crate::library::LibraryId,
) -> (SetupService, Arc<AccountDirectory>) {
    let library = match crate::library::LibraryLifecycle::open_with_id(root, library_id) {
        crate::library::LibraryOpenOutcome::Ready(library) => library,
        other => panic!("isolated test library did not open: {other:?}"),
    };
    let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
    storage
        .migrate(&[
            super::super::directory::MIGRATION,
            super::super::create::MIGRATION,
            super::super::delete::MIGRATION,
            crate::content::install::MIGRATION,
            crate::performance::mutation::MIGRATION,
            crate::accounts::directory::MIGRATION,
            crate::install::queue::MIGRATION,
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
    (service, accounts)
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

fn select_unowned_microsoft_account(accounts: &AccountDirectory) {
    use crate::accounts::{microsoft::MinecraftProfile, model::MicrosoftIdentity};

    let profile = uuid::Uuid::new_v4().simple().to_string();
    accounts
        .commit_microsoft(
            accounts.selection_revision().unwrap(),
            MicrosoftIdentity {
                login_id: uuid::Uuid::new_v4().to_string(),
                profile_id: profile.clone(),
                display_name: "UnownedPlayer".into(),
                credential_revision: 1,
                profile: MinecraftProfile {
                    id: profile,
                    name: "UnownedPlayer".into(),
                    skins: vec![],
                    capes: vec![],
                },
                owns_minecraft_java: false,
            },
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
    let mut service = Arc::new(service);
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
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .builds = vec![incompatible.clone()];
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
    select_unowned_microsoft_account(&accounts);
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
    assert_eq!(
        runtime_block.status_detail,
        "The selected Java executable is missing."
    );
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

    select_unowned_microsoft_account(&accounts);
    let unowned_account = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        unowned_account.error.unwrap().code,
        LaunchError::OnlineAccountUnavailable
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
    assert_eq!(
        serde_json::to_value(runtime.error.unwrap()).unwrap()["code"],
        "runtime_unavailable"
    );
    for catalog in unready_catalogs(&versions) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(
            blocked.status_detail,
            "The selected Java executable is missing."
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
async fn accepted_setup_fixture() -> (tempfile::TempDir, SetupService, SetupWork) {
    use crate::content::provenance::{ContentManifest, MANIFEST_FILE, ManifestEntry};
    use std::os::unix::fs::PermissionsExt;

    let (root, mut service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
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
    let client = ProviderClient::new(ClientConfig::default()).unwrap();
    let content = Arc::new(ContentService::new(client.clone()).unwrap());
    let mutations = Arc::new(ContentMutations::new(
        service.instances.directories().clone(),
        client,
        service.instances.tasks.clone(),
    ));
    let work = super::tests::pending_content_work(
        service.instances.clone(),
        content.clone(),
        mutations.clone(),
    )
    .await;
    let directory = work.instance().game_directory().read_projection().unwrap();
    let StoredSetupIntent::Content(stored) = &work.stored else {
        unreachable!()
    };
    let mut manifest = ContentManifest::default();
    for file in stored.artifacts.as_ref().unwrap() {
        manifest
            .try_upsert(
                ManifestEntry::managed(
                    file.canonical_id.clone(),
                    file.provider,
                    file.project_id.clone(),
                    file.version_id.clone(),
                    file.kind,
                    &file.file,
                    file.dependencies.clone(),
                    file.title.clone(),
                )
                .unwrap(),
            )
            .unwrap();
        std::fs::write(
            directory.join("resourcepacks").join(&file.file.filename),
            b"fixture",
        )
        .unwrap();
    }
    std::fs::write(
        directory.join(MANIFEST_FILE),
        manifest.encode_managed().unwrap(),
    )
    .unwrap();
    service.installs = Arc::new(
        service
            .installs
            .as_ref()
            .clone()
            .with_content(content.clone(), mutations.clone()),
    );
    (root, service.with_content(content, mutations), work)
}

#[cfg(unix)]
#[tokio::test]
async fn resumed_setup_response_is_ready_after_accepted_admission_releases() {
    let (_root, mut service, work) = accepted_setup_fixture().await;
    let instance = work.instance().record().instance.clone();
    let (restored, queue_id) =
        crate::install::queue::tests::interrupted_setup_fixture(&service.installs, &work).await;
    drop(work);
    let (content, mutations) = service.content.as_ref().unwrap();
    service.installs = Arc::new(restored.with_content(content.clone(), mutations.clone()));
    let service = Arc::new(service);
    let response = service.resume_setup(&instance.id).await.unwrap();
    assert!(!has_pending(service.instances.registry().storage(), &instance.id).unwrap());
    assert_eq!(
        service.installs.status(&queue_id).unwrap().outcome,
        Some(crate::install::model::InstallOutcome::Succeeded)
    );
    assert_eq!(
        response
            .install_queue
            .unwrap()
            .started_install
            .unwrap()
            .install_id,
        queue_id
    );
    let current = service
        .enrich(instance, &service.installed().await.unwrap())
        .await;
    assert!(current.launchable, "{}", current.status_detail);
    assert!(
        response.instance.launchable,
        "{}",
        response.instance.status_detail
    );
    assert_eq!(response.instance.launch_action.primary_action, "launch");
}

#[tokio::test]
async fn detached_setup_resumes_same_intent_after_ready_content_rollback() {
    detached_setup_ready_resume(false).await;
}

#[tokio::test]
async fn detached_setup_retains_intent_when_content_rollback_acknowledgement_is_refused() {
    detached_setup_ready_resume(true).await;
}

async fn detached_setup_ready_resume(refuse_acknowledgement: bool) {
    use crate::{
        content::{model::CanonicalId, packs::PackArchive, provenance::MANIFEST_FILE},
        instances::from_pack::PreparedPackCreation,
        library::LibraryId,
    };
    use std::{io::Write, path::Path, process::Command, time::Duration};

    const ROOT: &str = "AXIAL_SETUP_READY_CRASH_ROOT";
    const LIBRARY: &str = "AXIAL_SETUP_READY_CRASH_LIBRARY";
    const ORIGINAL: &[u8] = b"{\"schema_version\":3,\"entries\":[]}\n";

    fn open(root: &Path, library_id: LibraryId) -> Arc<SetupService> {
        let (mut service, _) = open_fixture(root, library_id);
        let client = ProviderClient::new(ClientConfig::default()).unwrap();
        let content = Arc::new(ContentService::new(client.clone()).unwrap());
        let mutations = Arc::new(ContentMutations::new(
            service.instances.directories().clone(),
            client,
            service.instances.tasks.clone(),
        ));
        service.installs = Arc::new(
            service
                .installs
                .as_ref()
                .clone()
                .with_content(content.clone(), mutations.clone()),
        );
        Arc::new(service.with_content(content, mutations))
    }

    fn intent(service: &SetupService, id: &InstanceId) -> (String, String, String) {
        service
            .instances
            .registry()
            .storage()
            .read(|db| {
                db.query_row(
                    "SELECT plan_id,request_json,phase FROM instance_setups WHERE instance_id=?1",
                    [id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(StorageError::from)
            })
            .unwrap()
    }

    fn content_receipt(service: &SetupService, id: &InstanceId) -> Option<String> {
        service.instances.registry().storage().read(|db| {
            db.query_row(
                "SELECT receipt_json FROM content_batches WHERE instance_id=?1 AND length(CAST(receipt_json AS BLOB))<=65536",
                [id.as_str()], |row| row.get(0),
            ).optional().map_err(StorageError::from)
        }).unwrap()
    }

    fn public_before(game: &Path) {
        assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), ORIGINAL);
        assert_eq!(
            std::fs::read(game.join("config/user.txt")).unwrap(),
            b"unrelated"
        );
        assert!(!game.join("config/accepted.txt").exists());
    }

    if let Some(root) = std::env::var_os(ROOT) {
        let root = std::path::PathBuf::from(root);
        assert_eq!(root.canonicalize().unwrap(), root);
        let service = open(
            &root,
            LibraryId::parse(&std::env::var(LIBRARY).unwrap()).unwrap(),
        );
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (path, bytes) in [
            ("modrinth.index.json", br#"{"formatVersion":1,"game":"minecraft","versionId":"fixture-v1","name":"Accepted fixture","dependencies":{"minecraft":"1.21.4"},"files":[]}"#.as_slice()),
            ("overrides/config/accepted.txt", b"accepted payload".as_slice()),
        ] {
            zip.start_file(path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let prepared = PreparedPackCreation::new(
            CanonicalId("modrinth:accepted-fixture".into()),
            "fixture-v1".into(),
            PackArchive::read(zip.finish().unwrap().into_inner()).unwrap(),
            "vanilla|1.21.4",
        )
        .unwrap();
        let stored: StoredSetupIntent = serde_json::from_value(serde_json::json!({
            "kind":"modpack", "canonical_id":"modrinth:accepted-fixture", "version_id":"fixture-v1",
            "selection_id":"vanilla|1.21.4", "installed_version_id":"1.21.4",
            "minecraft_version":"1.21.4", "loader_key":"vanilla",
            "archive_fingerprint":prepared.plan().fingerprint(),
            "install":{"kind":"vanilla", "version_id":"1.21.4"}
        }))
        .unwrap();
        let request_json = serde_json::to_string(&stored).unwrap();
        let admitted = service
            .instances
            .create_admitted(
                CreateInstanceRequest {
                    name: "Detached setup rollback".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    ..Default::default()
                },
                CreateTarget {
                    selection_id: "vanilla|1.21.4".into(),
                    version_id: "1.21.4".into(),
                    minecraft_version: "1.21.4".into(),
                    loader_key: "vanilla".into(),
                },
                service
                    .instances
                    .creation_admission_for_tests()
                    .await
                    .unwrap(),
                SetupIntent {
                    plan_id: uuid::Uuid::new_v4().to_string(),
                    request_json: request_json.clone(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let id = admitted.record().instance.id.clone();
        let game = root.join("instances").join(id.as_str());
        std::fs::write(game.join(MANIFEST_FILE), ORIGINAL).unwrap();
        std::fs::write(game.join("config/user.txt"), b"unrelated").unwrap();
        let accepted = intent(&service, &id);
        let (content, mutations) = service.content.as_ref().unwrap();
        let work = SetupWork {
            instances: service.instances.clone(),
            content: content.clone(),
            mutations: mutations.clone(),
            admitted,
            stored,
            request_json,
            prepared_pack: Some(prepared),
        };
        let observer = service.clone();
        let progress = Arc::new(move |event: axial_minecraft::DownloadProgress| {
            if event.phase != "content_commit" || event.current != 0 {
                return;
            }
            assert_eq!(event.total, 1);
            assert!(!event.done);
            assert_eq!(intent(&observer, &id), accepted);
            public_before(&game);
            let raw = content_receipt(&observer, &id).unwrap();
            let receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(receipt["native_settled"], false);
            assert!(
                receipt["ready_checkpoint"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
            );
            assert!(observer.content.as_ref().unwrap().1.has_unsettled_effects());
            std::process::exit(42);
        });
        let result = service
            .instances
            .tasks
            .try_spawn(work.clone(), move |cancel| async move {
                work.execute(&cancel, progress).await
            })
            .unwrap()
            .join()
            .await;
        panic!("accepted setup did not reach its Content Ready boundary: {result:?}");
    }

    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let library_id = LibraryId::new();
    let selector = if refuse_acknowledgement {
        "instances::setup::readiness_tests::detached_setup_retains_intent_when_content_rollback_acknowledgement_is_refused"
    } else {
        "instances::setup::readiness_tests::detached_setup_resumes_same_intent_after_ready_content_rollback"
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", selector, "--nocapture"])
        .env(ROOT, root.path())
        .env(LIBRARY, library_id.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let timed_out = child.try_wait().unwrap().is_none();
    if timed_out {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !timed_out && output.status.code() == Some(42),
        "Ready boundary not reached: {:?}; {} {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let service = open(root.path(), library_id);
    let records = service.instances.registry().list().unwrap();
    assert_eq!(records.len(), 1);
    let original = &records[0];
    let id = &original.instance.id;
    let game = root.path().join("instances").join(id.as_str());
    let accepted = intent(&service, id);
    assert_eq!(accepted.2, "pending");
    public_before(&game);
    assert!(!service.installs.has_setup_work(id));
    assert!(
        crate::content::install::has_pending(service.instances.registry().storage(), id).unwrap()
    );
    let original_receipt = content_receipt(&service, id).unwrap();
    let receipt: serde_json::Value = serde_json::from_str(&original_receipt).unwrap();
    let checkpoint = receipt["ready_checkpoint"].as_str().unwrap();
    axial_minecraft::managed_path::ManagedContentStagingCheckpoint::decode(checkpoint).unwrap();
    let checkpoint: serde_json::Value = serde_json::from_str(checkpoint).unwrap();
    let private = game.join(checkpoint["private_name"].as_str().unwrap());
    assert!(
        std::fs::symlink_metadata(&private)
            .unwrap()
            .file_type()
            .is_dir()
    );
    if refuse_acknowledgement {
        let operation = uuid::Uuid::parse_str(receipt["operation_id"].as_str().unwrap()).unwrap();
        service.instances.registry().storage().transaction(|db| {
            db.execute_batch(&format!(
                "CREATE TRIGGER refuse_setup_content_acknowledgement BEFORE DELETE ON content_batches WHEN OLD.instance_id='{id}' AND OLD.operation_id='{operation}' BEGIN SELECT RAISE(ABORT, 'injected Content acknowledgement refusal'); END;"
            )).map_err(StorageError::from)
        }).unwrap();
    }
    let stored: StoredSetupIntent = serde_json::from_str(&accepted.1).unwrap();
    let expected = stored.queue_request(&original.instance);
    let writer = service
        .instances
        .directories()
        .exclusions()
        .try_acquire(
            std::iter::empty::<String>(),
            [crate::install::queue::library_artifact(
                &library_id.to_string(),
            )],
        )
        .unwrap();
    let blocker = service
        .installs
        .enqueue(InstallQueueRequest::Vanilla {
            version_id: "1.20.1".into(),
        })
        .await
        .unwrap()
        .started_install
        .unwrap()
        .install_id;
    let response = tokio::time::timeout(Duration::from_secs(5), service.resume_setup(id)).await;
    let private_removed = matches!(
        std::fs::symlink_metadata(&private),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    let after = intent(&service, id);
    let content_pending =
        crate::content::install::has_pending(service.instances.registry().storage(), id).unwrap();
    let content_unsettled = service.content.as_ref().unwrap().1.has_unsettled_effects();
    let after_receipt = content_receipt(&service, id);
    let content_queue_rows: i64 = service
        .instances
        .registry()
        .storage()
        .read(|db| {
            db.query_row(
                "SELECT COUNT(*) FROM install_queue WHERE request_json=?1",
                [serde_json::to_string(&expected).unwrap()],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
        })
        .unwrap();
    let attached = service.installs.has_setup_work(id);
    let queued = service.installs.snapshot();
    service.installs.close_admission();
    service
        .instances
        .tasks
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), service.installs.join_observers())
        .await
        .unwrap()
        .unwrap();
    let shutdown = service.instances.tasks.shutdown_receipt().unwrap();
    service
        .content
        .as_ref()
        .unwrap()
        .1
        .release_shutdown_admissions(&shutdown)
        .unwrap();
    service.release_shutdown_admissions(&shutdown).unwrap();
    service.installs.shutdown_queued().unwrap();
    drop(writer);
    service
        .instances
        .directories()
        .library()
        .try_preserve()
        .unwrap();

    assert_eq!(after, accepted);
    assert_eq!(
        service.instances.registry().get_record(id).unwrap(),
        *original
    );
    public_before(&game);
    assert!(
        private_removed,
        "Ready rollback must remove its exact private directory before Content acknowledgement"
    );
    assert_eq!(content_receipt(&service, id), after_receipt);
    if refuse_acknowledgement {
        assert!(matches!(
            response.expect("bounded setup resume"),
            Err(InstanceError::SettlementRequired)
        ));
        assert!(content_pending && content_unsettled);
        assert_eq!(after_receipt.as_deref(), Some(original_receipt.as_str()));
        assert!(!attached && queued.active.is_none());
        assert_eq!(content_queue_rows, 0);
        assert_eq!(queued.items.len(), 1);
        assert_eq!(queued.items[0].queue_id, blocker);
        return;
    }
    assert!(
        !content_pending && !content_unsettled,
        "rollback must retire only the Content receipt"
    );
    assert_eq!(content_queue_rows, 1);
    let response = response
        .expect("setup resume must finish within its bound")
        .expect("proven Content rollback must requeue the same accepted setup");
    assert_eq!(response.instance.instance.id, *id);
    assert_eq!(response.view_model.state_id, "setup_queued");
    assert!(attached && queued.active.is_none());
    let started = response.install_queue.unwrap().started_install.unwrap();
    assert_ne!(started.install_id, blocker);
    let item = queued
        .items
        .iter()
        .find(|item| item.queue_id == started.install_id)
        .unwrap();
    let content = item.install_item.content.as_ref().unwrap();
    assert_eq!(
        InstallQueueRequest::Content {
            instance_id: content.instance_id.clone(),
            label: content.label.clone(),
            action: content.action.clone(),
        },
        expected
    );
    let status = service.installs.status(&started.install_id).unwrap();
    assert!(!status.done && status.outcome.is_none());
    assert_eq!(status.view_model.phase_id, "queued");
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_setup_response_still_invalidates_after_accepted_admission_releases() {
    let (_root, service, work) = accepted_setup_fixture().await;
    let instance = work.instance().record().instance.clone();
    let queued_instance = instance.clone();
    let queue = service.installs.clone();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let worker_release = release.clone();
    let (completed, completion) = tokio::sync::oneshot::channel();
    let task = service
        .instances
        .tasks
        .try_spawn(work.clone(), move |cancel| async move {
            work.execute(&cancel, Arc::new(|_| {})).await?;
            completed.send(()).unwrap();
            worker_release.acquire_owned().await.unwrap().forget();
            Ok((queued_instance, queue.snapshot(), setup_queued_result()))
        })
        .unwrap();
    let mut response = Box::pin(service.finish_setup(task));
    assert!(futures_util::poll!(&mut response).is_pending());
    completion.await.unwrap();
    let blocked = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(blocked.error.unwrap().code, LaunchError::InstanceBusy);
    let (before, mut changes) = service.installs.subscribe();
    drop(response);
    release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while changes.borrow_and_update().registry_revision == before.registry_revision {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let current = service
        .enrich(instance, &service.installed().await.unwrap())
        .await;
    assert!(current.launchable, "{}", current.status_detail);
}

#[cfg(unix)]
#[tokio::test]
async fn rejected_setup_result_invalidates_released_admission_without_claiming_success() {
    let (_root, service, work) = accepted_setup_fixture().await;
    let id = work.instance().record().instance.id.clone();
    let before = service.installs.snapshot();
    let task = service
        .instances
        .tasks
        .try_spawn(work, |_| async { Err(InstanceError::SetupUnavailable) })
        .unwrap();
    assert!(matches!(
        service.finish_setup(task).await,
        Err(InstanceError::SetupUnavailable)
    ));
    assert!(service.installs.snapshot().registry_revision > before.registry_revision);
    assert!(has_pending(service.instances.registry().storage(), &id).unwrap());
    assert!(
        service
            .instances
            .directories()
            .exclusions()
            .try_acquire([id.as_str()], [])
            .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn panicked_setup_keeps_admission_without_publishing_release() {
    let (_root, service, work) = accepted_setup_fixture().await;
    let id = work.instance().record().instance.id.clone();
    let before = service.installs.snapshot();
    let task = service
        .instances
        .tasks
        .try_spawn(work, |_| async {
            panic!("accepted setup interrupted before settlement");
        })
        .unwrap();
    assert!(matches!(
        service.finish_setup(task).await,
        Err(InstanceError::SettlementRequired)
    ));
    assert_eq!(
        service.installs.snapshot().registry_revision,
        before.registry_revision
    );
    assert!(
        service
            .instances
            .directories()
            .exclusions()
            .try_acquire([id.as_str()], [])
            .is_err()
    );
    assert_eq!(service.instances.tasks.status().unsettled.len(), 1);
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

#[cfg(unix)]
#[tokio::test]
async fn grouped_readiness_preserves_row_results_order_and_waiter_scope() {
    use std::os::unix::fs::PermissionsExt;

    let (root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    accounts.create_offline_account("GroupedPlayer").unwrap();
    let java = root.path().join("java");
    std::fs::write(&java, format!(
        "#!/bin/sh\nprobe_dir=${{0%/*}}\nprintf 'probe\\n' >> \"$probe_dir/probes\"\nwhile [ ! -f \"$probe_dir/probe-release\" ]; do sleep 0.01; done\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
        std::env::consts::ARCH,
    )).unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.path().join("probe-release"), b"release").unwrap();
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
    let first = super::super::create::tests::create(&service.instances, "First ready").await;
    let bad = super::super::create::tests::create(&service.instances, "Bad Java").await;
    let bad = service
        .instances
        .update(
            &bad.id,
            super::super::model::InstancePatch {
                java_path: Some(root.path().join("missing-java").to_str().unwrap().into()),
                ..Default::default()
            },
        )
        .unwrap();
    let last = super::super::create::tests::create(&service.instances, "Last ready").await;
    let missing = service
        .instances
        .create(
            CreateInstanceRequest {
                name: "Missing install".into(),
                selection_id: "vanilla|1.20.1".into(),
                ..Default::default()
            },
            CreateTarget {
                selection_id: "vanilla|1.20.1".into(),
                version_id: "1.20.1".into(),
                minecraft_version: "1.20.1".into(),
                loader_key: "vanilla".into(),
            },
            service
                .instances
                .creation_admission_for_tests()
                .await
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let input = vec![first.clone(), missing.clone(), bad.clone(), last.clone()];
    let versions = service.installed().await.unwrap();
    let rows = service.enrich_all(input.clone(), &versions).await;
    assert_eq!(
        rows.iter().map(|row| &row.instance.id).collect::<Vec<_>>(),
        input.iter().map(|row| &row.id).collect::<Vec<_>>()
    );
    assert!(rows[0].launchable && rows[3].launchable);
    assert_eq!(rows[1].launch_action.primary_action, "install");
    assert_eq!(rows[1].needs_install, "1.20.1");
    assert_eq!(rows[2].launch_action.primary_action, "blocked");
    assert_eq!(
        rows[2].status_detail,
        "The selected Java executable is missing."
    );
    assert_eq!(
        service
            .launch
            .fresh_install_checks
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert!(service.enrich(first.clone(), &versions).await.launchable);
    assert_eq!(
        service
            .launch
            .fresh_install_checks
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );

    // A dropped list finishes its accepted row, not the remaining group.
    std::fs::remove_file(root.path().join("probe-release")).unwrap();
    std::fs::write(root.path().join("probes"), b"").unwrap();
    let service = Arc::new(service);
    let waiter = tokio::spawn({
        let service = service.clone();
        async move { service.enrich_all(vec![first, last], &versions).await }
    });
    let started = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while std::fs::read(root.path().join("probes"))
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    waiter.abort();
    assert!(waiter.await.is_err_and(|error| error.is_cancelled()));
    let pin = service.instances.directories().library().admit().unwrap();
    let artifact = crate::install::queue::library_artifact(&pin.library_id().to_string());
    let blocked = service
        .instances
        .directories()
        .exclusions()
        .try_acquire(std::iter::empty::<String>(), [artifact.clone()])
        .is_err();
    std::fs::write(root.path().join("probe-release"), b"release").unwrap();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !service.instances.tasks.status().is_idle() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(started.is_ok() && blocked && drained.is_ok());
    assert!(!service.instances.tasks.status().closing);
    assert_eq!(
        std::fs::read_to_string(root.path().join("probes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(
        service
            .instances
            .directories()
            .exclusions()
            .try_acquire(std::iter::empty::<String>(), [artifact])
            .is_ok()
    );
}

#[tokio::test]
#[cfg(unix)]
async fn grouped_readiness_blocks_early_rows_when_the_library_scan_changes() {
    use futures_util::FutureExt;
    use std::os::unix::fs::PermissionsExt;

    let (root, service, accounts) = fixture();
    let service = Arc::new(service);
    let mut waiter = None;
    let journey = std::panic::AssertUnwindSafe(async {
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        accounts.create_offline_account("GroupedPlayer").unwrap();
        let java = root.path().join("java");
        std::fs::write(&java, format!(
            "#!/bin/sh\nprobe_dir=${{0%/*}}\nprintf 'probe\\n' >> \"$probe_dir/probes\"\nif [ -s \"$probe_dir/probe-first\" ]; then\n  while [ ! -s \"$probe_dir/probe-release\" ]; do sleep 0.01; done\nelse\n  printf 'first' > \"$probe_dir/probe-first\"\nfi\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        )).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.path().join("probe-first"), b"").unwrap();
        std::fs::write(root.path().join("probe-release"), b"release").unwrap();
        service.settings.update(serde_json::from_value(serde_json::json!({
            "expected_revision":service.settings.current().unwrap().revision,
            "java_path_override":java.to_str().unwrap(),
        })).unwrap()).unwrap();
        let first = super::super::create::tests::create(&service.instances, "First healthy").await;
        let last = super::super::create::tests::create(&service.instances, "Last healthy").await;
        let input = vec![first.clone(), last];
        let versions = service.installed().await.unwrap();
        let healthy = service.enrich_all(input.clone(), &versions).await;
        let protected: Vec<_> = ["json", "jar"].into_iter().map(|extension| {
            let path = root.path().join(format!("versions/1.21.4/1.21.4.{extension}"));
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        }).collect();
        let versions_root = root.path().join("versions");
        let revision_before = std::fs::metadata(&versions_root).unwrap().modified().unwrap();
        std::fs::write(root.path().join("probes"), b"").unwrap();
        std::fs::write(root.path().join("probe-first"), b"").unwrap();
        std::fs::write(root.path().join("probe-release"), b"").unwrap();
        waiter = Some(tokio::spawn({
            let service = service.clone();
            let input = input.clone();
            let versions = versions.clone();
            async move { service.enrich_all(input, &versions).await }
        }));
        // The first row has completed before the second actual Java probe pauses.
        let paused = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while std::fs::read_to_string(root.path().join("probes")).unwrap().lines().count() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await;
        let still_pending = !waiter.as_ref().unwrap().is_finished();
        let probe_count = std::fs::read_to_string(root.path().join("probes")).unwrap().lines().count();
        let external = versions_root.join("external-degraded-entry");
        let absent_before = std::fs::symlink_metadata(&external)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        std::fs::create_dir(&external).unwrap();
        let metadata = external.join("external-degraded-entry.json");
        let malformed = b"{not valid external version metadata\n";
        std::fs::write(&metadata, malformed).unwrap();
        let revision_after = std::fs::metadata(&versions_root).unwrap().modified().unwrap();
        std::fs::write(root.path().join("probe-release"), b"release").unwrap();
        let rows = waiter.take().unwrap().await.unwrap();
        let degraded = serde_json::to_value(service.launch.preflight(first.id.clone()).await).unwrap();
        let preserved = std::fs::read(&metadata).unwrap();
        std::fs::remove_file(&metadata).unwrap();
        std::fs::remove_dir(&external).unwrap();
        let versions = service.installed().await.unwrap();
        let restored = service.enrich_all(input.clone(), &versions).await;
        move || {
            assert!(paused.is_ok() && still_pending, "second probe did not remain pending");
            assert_eq!(probe_count, 2);
            assert!(absent_before);
            assert_ne!(revision_after, revision_before, "directory revision did not advance");
            assert_eq!(preserved, malformed);
            assert!(std::fs::symlink_metadata(external)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound));
            for (path, bytes) in protected {
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            for ready in [&healthy, &restored] {
                assert_eq!(ready.len(), input.len());
                for (row, instance) in ready.iter().zip(&input) {
                    assert_eq!(row.instance.id, instance.id);
                    assert!(row.launchable, "{}", row.status_detail);
                    assert!(row.launch_action.launchable);
                    assert_eq!(row.launch_action.primary_action, "launch");
                    assert!(row.needs_install.is_empty());
                }
            }
            assert_eq!(degraded["status"], "ready", "{degraded}");
            assert_eq!(degraded["launchable"], false);
            assert_eq!(degraded["readiness"], serde_json::json!({
                "launchable":false,"reasons":[{
                    "id":"installed_versions_degraded","severity":"blocking",
                    "message":"Could not verify installed versions. Check the library folder and try again."
                }]
            }));
            assert_eq!(rows.len(), input.len());
            for (row, instance) in rows.iter().zip(&input) {
                assert_eq!(row.instance.id, instance.id);
                assert!(!row.launchable, "{} retained stale Launch", instance.name);
                assert!(!row.launch_action.launchable);
                assert_eq!(row.launch_action.primary_action, "blocked");
                assert!(row.needs_install.is_empty());
            }
        }
    }).catch_unwind().await;
    let released = std::fs::write(root.path().join("probe-release"), b"release");
    let remaining = match waiter.take() {
        Some(waiter) => Some(waiter.await),
        None => None,
    };
    let shutdown = std::panic::AssertUnwindSafe(
        service
            .instances
            .tasks
            .shutdown(std::time::Duration::from_secs(5)),
    )
    .catch_unwind()
    .await;
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(released.is_ok());
        if let Some(result) = remaining {
            assert!(result.is_ok(), "enrichment waiter did not join");
        }
        match shutdown {
            Ok(result) => assert!(result.is_ok(), "{result:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
        assert!(service.instances.tasks.status().is_idle());
        match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }));
    if let Err(panic) = verification {
        eprintln!("Retained grouped scan fixture: {}", root.keep().display());
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn create_view_download_indicators_use_settled_install_and_scanner_status() {
    let (root, mut service, _) = fixture();
    service.loader_catalog_fixture = Some(stale_loader_catalog());
    let service = Arc::new(service);
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
    let mut service = Arc::new(service);
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
    let (target, request, _) = service.resolve(selection).await.unwrap();
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
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .builds = vec![newer, installed.clone()];
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
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .state
        .availability
        .fresh = true;
    Arc::get_mut(&mut service)
        .unwrap()
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
    Arc::get_mut(&mut service)
        .unwrap()
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
    Arc::get_mut(&mut service).unwrap().loader_catalog_fixture = Some(stale_loader_catalog());
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
