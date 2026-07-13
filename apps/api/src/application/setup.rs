//! Application-owned setup and onboarding workflows.

use crate::{
    application::instances::invalidate_create_view_root,
    state::{AppState, RequestProducerHandoff},
};
use axial_minecraft::create_minecraft_dir;
use axum::{Json, http::StatusCode};
use serde::Serialize;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Serialize)]
pub struct SetupLibraryResponse {
    pub status: &'static str,
    pub library_dir: String,
    pub library_mode: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub status: &'static str,
}

pub(crate) async fn setup_init_owned(
    state: &AppState,
    handoff: RequestProducerHandoff,
) -> Result<SetupLibraryResponse, ApiError> {
    let producer = handoff.try_claim().map_err(|_| setup_shutdown_error())?;
    let foreground = state
        .register_integrity_foreground()
        .map_err(|_| setup_shutdown_error())?;
    let transaction = producer.claim_child();
    let transaction_state = state.clone();
    transaction
        .spawn_joinable(async move {
            let foreground = foreground.wait_for_settlement().await;
            let target = transaction_state
                .managed_library_setup_target(&foreground)
                .map_err(setup_config_error)?;
            let blocking_library_dir = target.library_dir().to_path_buf();
            let filesystem_result =
                tokio::task::spawn_blocking(move || create_minecraft_dir(&blocking_library_dir))
                    .await;

            transaction_state.invalidate_installed_versions();
            invalidate_create_view_root(target.library_dir());
            filesystem_result
                .map_err(setup_managed_create_error)?
                .map_err(setup_managed_create_error)?;

            transaction_state
                .commit_managed_library_setup(&foreground, &target)
                .await
                .map_err(setup_config_error)?;
            transaction_state.invalidate_installed_versions();
            invalidate_create_view_root(target.library_dir());

            Ok(SetupLibraryResponse {
                status: "ok",
                library_dir: target.library_dir().to_string_lossy().into_owned(),
                library_mode: "managed",
            })
        })
        .await
        .map_err(|_| setup_transaction_error())?
}

pub async fn onboarding_complete(state: &AppState) -> Result<SetupStatusResponse, ApiError> {
    state
        .mutate_config(move |latest| {
            latest.onboarding_done = true;
            Ok(())
        })
        .await
        .map_err(onboarding_save_error)?;
    Ok(SetupStatusResponse { status: "ok" })
}

fn setup_managed_create_error(_error: impl std::fmt::Display) -> ApiError {
    internal_error(
        "Could not create the managed library folder. Check folder permissions and try again.",
    )
}

fn setup_config_error(_error: impl std::fmt::Display) -> ApiError {
    internal_error(
        "Could not save the managed library folder. Check app data permissions and try again.",
    )
}

fn onboarding_save_error(_error: impl std::fmt::Display) -> ApiError {
    internal_error("Could not save onboarding progress. Check app data permissions and try again.")
}

fn setup_shutdown_error() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "application shutdown is in progress" })),
    )
}

fn setup_transaction_error() -> ApiError {
    internal_error("Could not finish managed library setup. Try again.")
}

fn internal_error(message: &'static str) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::instances::{
            create_view_cache_contains_root_for_tests, seed_create_view_cache_for_tests,
        },
        state::{
            AppLifecyclePhase, AppStateInit, IdleSweepCancellation, IdleSweepReservation,
            IdleSweepTerminal, InstallStore, IntegrityForegroundLease, SessionStore,
        },
    };
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, ConfigStoreError, InstanceRegistrySnapshot, InstanceStore,
    };
    use axial_performance::PerformanceManager;
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn assert_bounded_setup_error(error: ApiError, expected_message: &str) {
        let (status, Json(body)) = error;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], expected_message);

        let rendered = body.to_string();
        assert!(!rendered.contains("/Users/alice/.axial"));
        assert!(!rendered.contains("permission denied"));
        assert!(!rendered.contains("config.toml"));
    }

    #[test]
    fn setup_managed_create_error_does_not_expose_raw_error_fragments() {
        assert_bounded_setup_error(
            setup_managed_create_error("permission denied creating /Users/alice/.axial/libraries"),
            "Could not create the managed library folder. Check folder permissions and try again.",
        );
    }

    #[test]
    fn setup_config_error_does_not_expose_raw_error_fragments() {
        assert_bounded_setup_error(
            setup_config_error("failed to write /Users/alice/.axial/config.toml"),
            "Could not save the managed library folder. Check app data permissions and try again.",
        );
    }

    #[test]
    fn setup_onboarding_save_error_does_not_expose_raw_error_fragments() {
        assert_bounded_setup_error(
            onboarding_save_error("permission denied writing /Users/alice/.axial/config.toml"),
            "Could not save onboarding progress. Check app data permissions and try again.",
        );
    }

    #[tokio::test]
    async fn setup_waits_for_cancelled_sweep_before_filesystem_or_config_effects() {
        let fixture = SetupFixture::new("sweep-settlement", |_| AppConfig::default());
        let (reservation, cancellation) = reserve_sweep(&fixture.state);
        let request = fixture
            .state
            .try_admit_request()
            .expect("admit setup request");
        let state = fixture.state.clone();
        let handoff = request.producer_handoff();
        let setup = tokio::spawn(async move { setup_init_owned(&state, handoff).await });

        wait_for_sweep_cancellation(&cancellation).await;
        assert!(!fixture.paths.library_dir.exists());
        assert!(fixture.state.config().current().library_dir.is_empty());
        assert!(!setup.is_finished());

        reservation.settle(IdleSweepTerminal::Cancelled);
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), setup)
            .await
            .expect("setup settles after sweep")
            .expect("setup task")
            .expect("setup succeeds");
        assert_eq!(response.library_mode, "managed");
        assert!(managed_layout_exists(&fixture.paths.library_dir));
        assert_eq!(
            fixture.state.config().current().library_dir,
            fixture.paths.library_dir.to_string_lossy().into_owned()
        );
        drop(request);
    }

    #[tokio::test]
    async fn partial_filesystem_failure_is_bounded_preserved_and_retryable() {
        let fixture = SetupFixture::new("partial-filesystem", |paths| AppConfig {
            library_dir: paths.library_dir.to_string_lossy().into_owned(),
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        });
        fs::create_dir_all(fixture.paths.library_dir.join("versions"))
            .expect("create cached versions root");
        fs::write(fixture.paths.library_dir.join("assets"), b"blocking file")
            .expect("block assets directory");
        refresh_installed_versions(&fixture.state).await;
        let walks_before_failure = fixture.state.installed_versions_walk_count();
        seed_create_view_cache_for_tests(&fixture.paths.library_dir);
        assert!(create_view_cache_contains_root_for_tests(
            &fixture.paths.library_dir
        ));

        let error = run_setup(&fixture.state)
            .await
            .expect_err("partial layout must fail");
        assert_bounded_setup_error(
            error,
            "Could not create the managed library folder. Check folder permissions and try again.",
        );
        assert!(fixture.paths.library_dir.join("versions").is_dir());
        assert!(fixture.paths.library_dir.join("libraries").is_dir());
        assert!(fixture.paths.library_dir.join("assets").is_file());
        let visible = fixture.state.config().current();
        assert_eq!(visible.library_mode, "existing");
        assert_eq!(
            visible.library_dir,
            fixture.paths.library_dir.to_string_lossy().into_owned()
        );
        assert!(!create_view_cache_contains_root_for_tests(
            &fixture.paths.library_dir
        ));
        refresh_installed_versions(&fixture.state).await;
        assert!(fixture.state.installed_versions_walk_count() > walks_before_failure);

        fs::remove_file(fixture.paths.library_dir.join("assets")).expect("remove blocking file");
        run_setup(&fixture.state)
            .await
            .expect("retry repairs partial layout");
        assert!(managed_layout_exists(&fixture.paths.library_dir));
        assert_eq!(
            fixture.state.config().current().library_dir,
            fixture.paths.library_dir.to_string_lossy().into_owned()
        );
        assert_eq!(fixture.state.config().current().library_mode, "managed");
    }

    #[tokio::test]
    async fn config_failure_preserves_layout_old_visibility_and_cache_fences_through_retry() {
        let fixture = SetupFixture::new("config-failure", |paths| AppConfig {
            library_dir: paths.library_dir.to_string_lossy().into_owned(),
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        });
        refresh_installed_versions(&fixture.state).await;
        seed_create_view_cache_for_tests(&fixture.paths.library_dir);
        assert!(create_view_cache_contains_root_for_tests(
            &fixture.paths.library_dir
        ));
        block_config_destination(&fixture.paths.config_file);

        let error = run_setup(&fixture.state)
            .await
            .expect_err("blocked config commit must fail");
        assert_bounded_setup_error(
            error,
            "Could not save the managed library folder. Check app data permissions and try again.",
        );
        assert!(managed_layout_exists(&fixture.paths.library_dir));
        let visible = fixture.state.config().current();
        assert_eq!(visible.library_mode, "existing");
        assert_eq!(
            visible.library_dir,
            fixture.paths.library_dir.to_string_lossy().into_owned()
        );
        assert!(!create_view_cache_contains_root_for_tests(
            &fixture.paths.library_dir
        ));

        let walks_before_repopulate = fixture.state.installed_versions_walk_count();
        refresh_installed_versions(&fixture.state).await;
        assert!(fixture.state.installed_versions_walk_count() > walks_before_repopulate);
        let walks_before_retry = fixture.state.installed_versions_walk_count();

        fs::remove_dir_all(&fixture.paths.config_file).expect("unblock config destination");
        fixture
            .state
            .mutate_config(|latest| {
                latest.theme = "after-setup-retry".to_string();
                Ok(())
            })
            .await
            .expect("successor reconciles retained setup config");
        let visible = fixture.state.config().current();
        assert_eq!(visible.library_mode, "managed");
        assert_eq!(visible.theme, "after-setup-retry");
        refresh_installed_versions(&fixture.state).await;
        assert!(fixture.state.installed_versions_walk_count() > walks_before_retry);
    }

    #[tokio::test]
    async fn same_root_setup_repairs_layout_and_invalidates_both_caches() {
        let fixture = SetupFixture::new("same-root", |paths| AppConfig {
            library_dir: paths.library_dir.to_string_lossy().into_owned(),
            library_mode: "managed".to_string(),
            ..AppConfig::default()
        });
        fs::create_dir_all(fixture.paths.library_dir.join("versions"))
            .expect("create partial same-root layout");
        refresh_installed_versions(&fixture.state).await;
        let walks_before = fixture.state.installed_versions_walk_count();
        seed_create_view_cache_for_tests(&fixture.paths.library_dir);

        run_setup(&fixture.state)
            .await
            .expect("same-root setup repairs layout");
        assert!(managed_layout_exists(&fixture.paths.library_dir));
        assert!(!create_view_cache_contains_root_for_tests(
            &fixture.paths.library_dir
        ));
        refresh_installed_versions(&fixture.state).await;
        assert!(fixture.state.installed_versions_walk_count() > walks_before);
    }

    #[tokio::test]
    async fn dropped_setup_caller_remains_owned_through_config_gate_and_quiescence() {
        let fixture = SetupFixture::new("caller-drop", |_| AppConfig::default());
        let config_gate = fixture
            .state
            .config()
            .acquire_mutation()
            .await
            .expect("hold config mutation gate");
        let request = fixture
            .state
            .try_admit_request()
            .expect("admit setup request");
        let mut setup = Box::pin(setup_init_owned(&fixture.state, request.producer_handoff()));
        poll_pending(setup.as_mut());
        wait_for_managed_layout(&fixture.paths.library_dir).await;
        assert!(fixture.state.config().current().library_dir.is_empty());

        drop(setup);
        drop(request);
        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        wait_for_phase(&fixture.state, AppLifecyclePhase::QuiescingProducers).await;
        assert!(!quiesce.is_finished());
        drop(config_gate);
        tokio::time::timeout(std::time::Duration::from_secs(5), quiesce)
            .await
            .expect("setup owner drains")
            .expect("quiesce task")
            .expect("quiesce succeeds");
        assert_eq!(
            fixture.state.config().current().library_dir,
            fixture.paths.library_dir.to_string_lossy().into_owned()
        );
    }

    #[tokio::test]
    async fn foreign_foreground_rejects_setup_target_and_commit_before_config_effect() {
        let owner = SetupFixture::new("foreign-owner", |_| AppConfig::default());
        let target = SetupFixture::new("foreign-target", |_| AppConfig::default());
        let foreign = foreground(&owner.state).await;
        let error = match target.state.managed_library_setup_target(&foreign) {
            Ok(_) => panic!("foreign foreground cannot derive setup target"),
            Err(error) => error,
        };
        assert_foreign_foreground_error(error);

        let target_foreground = foreground(&target.state).await;
        let setup_target = target
            .state
            .managed_library_setup_target(&target_foreground)
            .expect("derive own setup target");
        let error = target
            .state
            .commit_managed_library_setup(&foreign, &setup_target)
            .await
            .expect_err("foreign foreground cannot commit setup config");
        assert_foreign_foreground_error(error);
        assert!(target.state.config().current().library_dir.is_empty());
    }

    #[tokio::test]
    async fn setup_commit_preserves_an_unrelated_concurrent_config_update() {
        let fixture = SetupFixture::new("concurrent-config", |_| AppConfig::default());
        let foreground = foreground(&fixture.state).await;
        let target = fixture
            .state
            .managed_library_setup_target(&foreground)
            .expect("derive setup target");
        let gate = fixture
            .state
            .config()
            .acquire_mutation()
            .await
            .expect("hold config gate");
        let mut unrelated = Box::pin(fixture.state.mutate_config(|latest| {
            latest.theme = "concurrent-theme".to_string();
            Ok(())
        }));
        poll_pending(unrelated.as_mut());
        let mut setup = Box::pin(
            fixture
                .state
                .commit_managed_library_setup(&foreground, &target),
        );
        poll_pending(setup.as_mut());
        drop(gate);

        unrelated.await.expect("unrelated update commits first");
        setup.await.expect("setup derives from latest config");
        let visible = fixture.state.config().current();
        assert_eq!(visible.theme, "concurrent-theme");
        assert_eq!(visible.library_mode, "managed");
        assert_eq!(
            visible.library_dir,
            fixture.paths.library_dir.to_string_lossy().into_owned()
        );
    }

    #[tokio::test]
    async fn admitted_setup_handoff_finishes_during_request_drain() {
        let fixture = SetupFixture::new("request-drain", |_| AppConfig::default());
        let request = fixture
            .state
            .try_admit_request()
            .expect("admit setup request");
        let handoff = request.producer_handoff();
        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        wait_for_phase(&fixture.state, AppLifecyclePhase::DrainingRequests).await;
        assert!(fixture.state.try_claim_producer().is_err());

        let response = setup_init_owned(&fixture.state, handoff)
            .await
            .expect("admitted setup completes during request drain");
        assert_eq!(response.library_mode, "managed");
        assert!(managed_layout_exists(&fixture.paths.library_dir));
        drop(request);
        tokio::time::timeout(std::time::Duration::from_secs(5), quiesce)
            .await
            .expect("request drain completes")
            .expect("quiesce task")
            .expect("quiesce succeeds");
    }

    struct SetupFixture {
        state: AppState,
        paths: AppPaths,
        root: PathBuf,
    }

    impl SetupFixture {
        fn new(name: &str, config: impl FnOnce(&AppPaths) -> AppConfig) -> Self {
            let root = unique_test_dir(name);
            let paths = test_paths(&root);
            let config = Arc::new(
                ConfigStore::from_config(paths.clone(), config(&paths)).expect("config source"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("instance source"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(&paths.config_dir)
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });
            Self { state, paths, root }
        }
    }

    impl Drop for SetupFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    async fn run_setup(state: &AppState) -> Result<SetupLibraryResponse, ApiError> {
        let request = state.try_admit_request().expect("admit setup request");
        let result = setup_init_owned(state, request.producer_handoff()).await;
        drop(request);
        result
    }

    fn reserve_sweep(state: &AppState) -> (IdleSweepReservation, IdleSweepCancellation) {
        let epoch = state.subscribe_integrity_idle().borrow().epoch();
        let reservation = state
            .try_reserve_idle_sweep(
                epoch,
                state.try_claim_producer().expect("claim sweep producer"),
            )
            .expect("reserve setup sweep");
        let cancellation = reservation.cancellation();
        (reservation, cancellation)
    }

    async fn wait_for_sweep_cancellation(cancellation: &IdleSweepCancellation) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !cancellation.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("setup foreground cancels sweep");
    }

    async fn wait_for_phase(state: &AppState, expected: AppLifecyclePhase) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while state.lifecycle_phase() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("application reaches expected phase");
    }

    async fn wait_for_managed_layout(library_dir: &Path) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !managed_layout_exists(library_dir) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("managed layout is prepared");
    }

    fn managed_layout_exists(library_dir: &Path) -> bool {
        ["versions", "libraries", "assets", "cache/loaders/catalog"]
            .iter()
            .all(|subdir| library_dir.join(subdir).is_dir())
    }

    async fn refresh_installed_versions(state: &AppState) {
        let producer = state
            .try_claim_producer()
            .expect("claim installed-version producer");
        state
            .installed_versions_snapshot(&producer)
            .await
            .expect("configured installed-version snapshot");
    }

    async fn foreground(state: &AppState) -> IntegrityForegroundLease {
        state
            .register_integrity_foreground()
            .expect("register setup foreground")
            .wait_for_settlement()
            .await
    }

    fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
        let waker = futures_util::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut context),
            std::task::Poll::Pending
        ));
    }

    fn block_config_destination(config_file: &Path) {
        if config_file.is_file() {
            fs::remove_file(config_file).expect("remove config file before blocking");
        }
        fs::create_dir_all(config_file).expect("block config destination with directory");
    }

    fn assert_foreign_foreground_error(error: ConfigStoreError) {
        let ConfigStoreError::Persistence(error) = error else {
            panic!("foreign setup foreground must fail as persistence permission denial");
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("managed-library"),
            config_dir,
        }
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-api-setup-{name}-{}-{nanos}",
            std::process::id()
        ))
    }
}
