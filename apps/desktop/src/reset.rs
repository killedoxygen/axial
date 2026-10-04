//! Development reset keeps the admitted root until the desktop and service
//! owners have dropped. Files are cleared only through retained root authority.
use crate::{
    bootstrap::DesktopBootstrap,
    lifecycle::{DesktopLifecycle, TerminalIntent},
};
use axial_app::{
    library::{ApplicationRootPin, LibraryId, LibraryLifecycle},
    public::DEVELOPMENT_APPLICATION_ID,
    tasks::TaskOwner,
};
use axial_fs::{LeafName, PendingRootReset, RootSession};
use serde::Deserialize;
use std::{
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, State, WebviewWindow};

const UNAVAILABLE: &str = "Developer reset is unavailable in this build.";
const PREFLIGHT_FAILED: &str =
    "Reset is blocked because launcher-owned storage could not be proven safe.";
const DELETE_FAILED: &str =
    "Reset is incomplete because launcher-owned data could not be deleted. Try again.";
const PROFILE_MARKER: &str = ".axial-rewrite-profile";
const PROFILE_MARKER_LIMIT: u64 = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileIdentity {
    application_id: String,
    profile_id: String,
}

struct ResetState {
    pin: Option<ApplicationRootPin>,
    requested: bool,
    transferred: bool,
    ingress_closed: bool,
    active_requests: usize,
}

#[derive(Clone)]
pub struct NativeReset {
    library: LibraryLifecycle,
    tasks: TaskOwner,
    root: PathBuf,
    marker: Arc<Vec<u8>>,
    state: Arc<Mutex<ResetState>>,
    attempt: Arc<tokio::sync::Mutex<()>>,
    changed: Arc<tokio::sync::Notify>,
}

impl NativeReset {
    /// Composition supplies the already admitted replacement library, never a
    /// caller-selected deletion path. Normal startup retains no additional pin.
    pub fn new(library: LibraryLifecycle, tasks: TaskOwner) -> Result<Self, String> {
        if !cfg!(debug_assertions) {
            return Err(UNAVAILABLE.into());
        }
        let pin = library
            .admit_application_root()
            .map_err(|_| PREFLIGHT_FAILED)?;
        let root = pin.read_projection().map_err(|_| PREFLIGHT_FAILED)?;
        if !independent_root(&root) {
            return Err(PREFLIGHT_FAILED.into());
        }
        let marker = read_marker(&pin, &root)?;
        let identity: ProfileIdentity =
            serde_json::from_slice(&marker).map_err(|_| PREFLIGHT_FAILED)?;
        let profile_id = LibraryId::parse(&identity.profile_id).map_err(|_| PREFLIGHT_FAILED)?;
        if identity.application_id != DEVELOPMENT_APPLICATION_ID
            || profile_id.to_string() == "00000000-0000-0000-0000-000000000000"
        {
            return Err(PREFLIGHT_FAILED.into());
        }
        drop(pin);
        Ok(Self {
            library,
            tasks,
            root,
            marker: Arc::new(marker),
            state: Arc::new(Mutex::new(ResetState {
                pin: None,
                requested: false,
                transferred: false,
                ingress_closed: false,
                active_requests: 0,
            })),
            attempt: Arc::new(tokio::sync::Mutex::new(())),
            changed: Arc::new(tokio::sync::Notify::new()),
        })
    }

    async fn prepare(&self, lifecycle: &DesktopLifecycle) -> Result<bool, String> {
        let _attempt = self.attempt.lock().await;
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.requested {
                return Ok(false);
            }
            self.library
                .ensure_no_interrupted_launch()
                .map_err(|_| PREFLIGHT_FAILED)?;
            if state.pin.is_none() {
                state.pin = Some(
                    self.library
                        .admit_application_root()
                        .map_err(|_| PREFLIGHT_FAILED)?,
                );
            }
            // Do this before terminal admission, so failed preflight leaves the
            // working app and its API available. Retain the pin through shutdown:
            // ordinary preservation must not revoke the session needed by reset.
            let pin = state.pin.as_ref().expect("reset retains its root pin");
            if read_marker(pin, &self.root)? != *self.marker {
                return Err(PREFLIGHT_FAILED.into());
            }
        }
        lifecycle.prepare_exit(TerminalIntent::Reset).await?;
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requested = true;
        Ok(true)
    }

    /// Close ingress and join every synchronously accepted request, including a
    /// worker not yet polled when the native event loop unexpectedly returns.
    pub async fn quiesce_after_exit(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .ingress_closed = true;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active_requests
                == 0
            {
                return;
            }
            changed.await;
        }
    }

    /// Call after `run_return`, then drop DesktopServices, DesktopLifecycle and
    /// every AppHandle before attempting deletion. Those owners hold metadata
    /// connections and long-lived skin/runtime capabilities even after shutdown.
    /// This transfer can succeed once and only after reset preparation joined.
    pub fn take_after_exit(&self) -> Option<PendingReset> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.requested || state.transferred || state.active_requests != 0 {
            return None;
        }
        state.transferred = true;
        Some(PendingReset {
            library: Some(self.library.clone()),
            tasks: Some(self.tasks.clone()),
            pin: state.pin.take(),
            root: self.root.clone(),
            marker: self.marker.clone(),
            pending: None,
            startup_session: None,
        })
    }
}

fn independent_root(root: &Path) -> bool {
    root.is_absolute()
        && root.parent().is_some()
        && !root.components().any(|part| {
            matches!(part, Component::ParentDir | Component::CurDir)
                || matches!(part, Component::Normal(name)
                    if name == "dev.mateoltd.axial" || name == "com.mateoltd.axial")
        })
}

fn read_marker(pin: &ApplicationRootPin, expected_root: &Path) -> Result<Vec<u8>, String> {
    if pin.read_projection().map_err(|_| PREFLIGHT_FAILED)? != expected_root {
        return Err(PREFLIGHT_FAILED.into());
    }
    pin.admit_native_file(&expected_root.join(PROFILE_MARKER), PROFILE_MARKER_LIMIT)
        .and_then(|file| file.read())
        .map_err(|_| PREFLIGHT_FAILED.into())
}

/// The integration owner retains this value across errors and retries. Native
/// reset outcomes must never be dropped as if a failed deletion had no effect.
#[must_use = "retain pending reset until its exact root clear and lease release succeed"]
pub struct PendingReset {
    library: Option<LibraryLifecycle>,
    tasks: Option<TaskOwner>,
    pin: Option<ApplicationRootPin>,
    root: PathBuf,
    marker: Arc<Vec<u8>>,
    pending: Option<PendingRootReset>,
    startup_session: Option<RootSession>,
}

impl PendingReset {
    /// Only the interrupted-startup confirmation path supplies this already
    /// admitted session. Validation errors retain it in the ordinary retry owner.
    pub fn from_confirmed_startup(session: RootSession) -> Self {
        Self {
            library: None,
            tasks: None,
            pin: None,
            root: PathBuf::new(),
            marker: Arc::new(Vec::new()),
            pending: None,
            startup_session: Some(session),
        }
    }

    /// Run on the blocking pool after all application/service owners have been
    /// dropped. A failure keeps exact authority for another call. Neither a live
    /// task, escaped capability nor receipt is bypassed by retrying this method.
    pub fn try_clear(&mut self) -> Result<(), String> {
        if self
            .tasks
            .as_ref()
            .is_some_and(|tasks| tasks.shutdown_receipt().is_none())
        {
            return Err(DELETE_FAILED.into());
        }
        if let Some(pin) = self.pin.as_ref() {
            if read_marker(pin, &self.root)? != *self.marker {
                return Err(PREFLIGHT_FAILED.into());
            }
            self.pin.take();
        }

        if self.pending.is_none() {
            if let Some(session) = self.startup_session.as_ref() {
                let marker = session
                    .root()
                    .and_then(|root| {
                        root.open_file(&LeafName::new(PROFILE_MARKER).expect("fixed marker name"))
                    })
                    .and_then(|file| file.read_bounded(PROFILE_MARKER_LIMIT))
                    .map_err(|_| PREFLIGHT_FAILED)?;
                let identity: ProfileIdentity =
                    serde_json::from_slice(&marker).map_err(|_| PREFLIGHT_FAILED)?;
                let id = LibraryId::parse(&identity.profile_id).map_err(|_| PREFLIGHT_FAILED)?;
                if identity.application_id != DEVELOPMENT_APPLICATION_ID
                    || id.to_string() == "00000000-0000-0000-0000-000000000000"
                {
                    return Err(PREFLIGHT_FAILED.into());
                }
                if !session
                    .interrupted_reset(&LeafName::new(PROFILE_MARKER).expect("fixed marker name"))
                    .map_err(|_| PREFLIGHT_FAILED)?
                {
                    return Err(PREFLIGHT_FAILED.into());
                }
                self.marker = Arc::new(marker);
            }
            let session = match self.startup_session.take() {
                Some(session) => session,
                None => self
                    .library
                    .as_ref()
                    .ok_or(DELETE_FAILED)?
                    .take_reset_session()
                    .map_err(|error| {
                        // LibraryError's display is a bounded cause, without a
                        // profile path or account data. Keep the public error terse.
                        tracing::warn!(cause = %error, "Reset is waiting for application-root ownership");
                        DELETE_FAILED
                    })?,
            };
            self.pending = Some(PendingRootReset::new(
                session,
                LeafName::new(PROFILE_MARKER).expect("fixed marker name"),
                (*self.marker).clone(),
            ));
        }
        self.pending
            .as_mut()
            .expect("reset retains native owner")
            .try_clear()
            .map_err(|_| DELETE_FAILED.into())
    }
}

struct ResetRequest {
    reset: NativeReset,
    lifecycle: Option<DesktopLifecycle>,
    exit: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for ResetRequest {
    fn drop(&mut self) {
        // The completion notification must follow native-handle and service
        // release even on error or unwind, not merely precede worker completion.
        self.exit.take();
        self.lifecycle.take();
        let mut state = self
            .reset
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active_requests -= 1;
        self.reset.changed.notify_waiters();
    }
}

fn request_reset(
    reset: NativeReset,
    lifecycle: DesktopLifecycle,
    exit: impl FnOnce() + Send + 'static,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let reservation = {
        let mut state = reset
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.ingress_closed {
            None
        } else {
            state.active_requests += 1;
            Some(ResetRequest {
                reset: reset.clone(),
                lifecycle: Some(lifecycle),
                exit: Some(Box::new(exit)),
            })
        }
    };
    // Acceptance and the native side effect survive a disconnected IPC waiter.
    tokio::spawn(async move {
        let mut reservation = reservation.ok_or_else(|| DELETE_FAILED.to_string())?;
        let lifecycle = reservation
            .lifecycle
            .as_ref()
            .expect("request retains lifecycle");
        if reservation.reset.prepare(lifecycle).await? {
            lifecycle.allow_exit();
            reservation
                .exit
                .take()
                .expect("request retains exit callback")();
        }
        Ok(())
    })
}

#[tauri::command]
pub async fn app_reset(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    lifecycle: State<'_, DesktopLifecycle>,
    reset: State<'_, NativeReset>,
) -> Result<(), String> {
    if !cfg!(debug_assertions) {
        return Err(UNAVAILABLE.into());
    }
    crate::window::require_main_window(&window, &bootstrap)?;
    request_reset(
        reset.inner().clone(),
        lifecycle.inner().clone(),
        move || {
            // Tauri request_restart restarts before run_return drops service owners.
            // Main performs the restart only after PendingReset releases its receipt.
            app.exit(0);
        },
    )
    .await
    .map_err(|_| DELETE_FAILED.to_string())?
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::{discord_presence::PresenceObserver, native_skin::NativeSkinFiles};
    use axial_app::library::AdmissionState;
    use axial_app::{
        launch::{coordinator::LaunchIntents, reports::LaunchReportStore},
        storage::StorageError,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn lifecycle_for(services: &axial_api::DesktopServices) -> DesktopLifecycle {
        DesktopLifecycle::new(
            services.tasks.clone(),
            services.server.clone(),
            PresenceObserver::disabled_for_test(),
            NativeSkinFiles::new(services.library.clone(), services.tasks.clone()),
            services.skins.clone(),
        )
    }

    async fn fixture() -> (
        tempfile::TempDir,
        axial_api::DesktopServices,
        DesktopLifecycle,
        NativeReset,
    ) {
        let temporary =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let services = axial_api::start_in_profile(temporary.path().join("rewrite"), None)
            .await
            .unwrap();
        let lifecycle = lifecycle_for(&services).without_interface_preferences_for_test();
        let reset = NativeReset::new(services.library.clone(), services.tasks.clone()).unwrap();
        (temporary, services, lifecycle, reset)
    }

    const UNBOUND_INTENT: &str = "d74a9bd4-d1eb-4122-b705-2f50ac9212e5";

    fn fence_unbound_launch(services: &axial_api::DesktopServices) -> Vec<u8> {
        let payload = serde_json::to_vec(&serde_json::json!({
            "request": {
                "instance_id": "a9d33868-e694-41f6-9b91-f5989e5f4152",
                "intent_key": UNBOUND_INTENT,
            },
            "context": null,
            "session_id": "e73780e5-964c-4382-b421-d07c8aa1c43f",
        }))
        .unwrap();
        let storage = services.settings.metadata().clone();
        storage
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute(
                    "INSERT INTO launch_intents(intent_key,payload,state) VALUES(?1,?2,'accepted')",
                    axial_app::storage::rusqlite::params![UNBOUND_INTENT, &payload],
                )?;
                Ok(())
            })
            .unwrap();
        // An old accepted row cannot restore physical authority, but its failed
        // restoration must still fence destructive lifecycle admission.
        assert!(
            LaunchIntents::restore(
                storage.clone(),
                LaunchReportStore::new(storage).unwrap(),
                services.instances.directories(),
            )
            .is_err()
        );
        assert!(services.library.ensure_no_interrupted_launch().is_err());
        payload
    }

    fn assert_unbound_launch_preserved(services: &axial_api::DesktopServices, payload: &[u8]) {
        let row = services
            .settings
            .metadata()
            .read(|db| -> Result<_, StorageError> {
                Ok(db.query_row(
                    "SELECT payload,state,terminal_ack,settlement,(SELECT count(*) FROM launch_reports)
                     FROM launch_intents WHERE intent_key=?1",
                    [UNBOUND_INTENT],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, Option<Vec<u8>>>(3)?,
                            row.get::<_, usize>(4)?,
                        ))
                    },
                )?)
            })
            .unwrap();
        assert_eq!(row, (payload.to_vec(), "accepted".into(), false, None, 0));
        assert!(services.library.ensure_no_interrupted_launch().is_err());
    }

    #[tokio::test]
    async fn interrupted_launch_blocks_reset_before_pin_and_terminal_admission() {
        let (_temporary, services, lifecycle, reset) = fixture().await;
        let payload = fence_unbound_launch(&services);
        let result = reset.prepare(&lifecycle).await;
        let retained_pin = reset.state.lock().unwrap().pin.is_some();
        let closing = services.tasks.status().closing;
        let admission = services.library.snapshot().admission;
        let settled = services.server.is_shutdown_settled();
        let close = lifecycle.prepare_exit(TerminalIntent::Close).await;
        services.library.try_preserve().unwrap();

        assert_eq!(result, Err(PREFLIGHT_FAILED.into()));
        assert!(!retained_pin);
        assert!(!closing);
        assert_eq!(admission, AdmissionState::Open);
        assert!(!settled);
        assert!(reset.take_after_exit().is_none());
        close.unwrap();
        assert_unbound_launch_preserved(&services, &payload);
    }

    #[tokio::test]
    async fn interrupted_launch_blocks_destructive_intents_before_preferences_but_allows_close() {
        let (_temporary, services, _, reset) = fixture().await;
        let lifecycle = lifecycle_for(&services);
        let payload = fence_unbound_launch(&services);
        let mut events = lifecycle.interface_preferences_events();
        let mut refusals = Vec::new();
        for intent in [
            TerminalIntent::Reset,
            TerminalIntent::Restart,
            TerminalIntent::Update,
        ] {
            let owner = lifecycle.clone();
            let mut request = tokio::spawn(async move { owner.prepare_exit(intent).await });
            let (result, preferences_requested) = tokio::select! {
                result = &mut request => (result.unwrap(), false),
                _ = events.changed() => {
                    let event = events.borrow_and_update().clone().unwrap();
                    lifecycle.interface_preferences_delivery_failed(&event);
                    (request.await.unwrap(), true)
                }
            };
            events.borrow_and_update();
            refusals.push((intent, result, preferences_requested));
        }
        let closing = services.tasks.status().closing;
        let admission = services.library.snapshot().admission;
        let settled = services.server.is_shutdown_settled();
        let url = tauri::Url::parse(&services.server.bootstrap().base_url).unwrap();
        let address = ("127.0.0.1", url.port().unwrap());
        let available = tokio::net::TcpStream::connect(address).await.is_ok();
        let close = lifecycle
            .clone()
            .without_interface_preferences_for_test()
            .prepare_exit(TerminalIntent::Close)
            .await;
        services.library.try_preserve().unwrap();

        for (intent, result, preferences_requested) in refusals {
            assert!(result.is_err(), "{intent:?}");
            assert!(!preferences_requested, "{intent:?}");
        }
        assert!(!closing);
        assert_eq!(admission, AdmissionState::Open);
        assert!(!settled);
        assert!(available);
        close.unwrap();
        assert!(services.server.is_shutdown_settled());
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        assert!(reset.take_after_exit().is_none());
        assert!(services.library.begin_switch().is_err());
        assert!(services.library.take_reset_session().is_err());
        assert!(services.library.revoke_application_root().is_err());
        assert_unbound_launch_preserved(&services, &payload);
    }

    #[tokio::test]
    async fn reset_waits_for_services_and_escaped_file_admission_then_clears_only_its_profile() {
        let (temporary, services, lifecycle, reset) = fixture().await;
        let root = services.profile_root.clone();
        let original_marker = std::fs::read(root.join(PROFILE_MARKER)).unwrap();
        let baseline = temporary.path().join("baseline");
        std::fs::create_dir(&baseline).unwrap();
        std::fs::write(baseline.join("keep.txt"), b"baseline user data").unwrap();
        std::fs::write(root.join("reset.txt"), b"development data").unwrap();
        let pin = services.library.admit_application_root().unwrap();
        let escaped = pin.admit_native_file(&root.join("reset.txt"), 128).unwrap();
        drop(pin);
        assert!(reset.prepare(&lifecycle).await.unwrap());
        assert!(!reset.prepare(&lifecycle).await.unwrap());
        let mut pending = reset.take_after_exit().unwrap();
        assert!(reset.take_after_exit().is_none());
        assert!(pending.try_clear().is_err());
        assert!(root.join("metadata.sqlite").exists());
        drop(lifecycle);
        drop(services);
        assert!(pending.try_clear().is_err());
        assert_eq!(escaped.read().unwrap(), b"development data");
        pending.try_clear().unwrap();
        pending.try_clear().unwrap();
        assert!(!root.join("reset.txt").exists());
        assert!(!root.join("metadata.sqlite").exists());
        let fresh_marker = std::fs::read(root.join(PROFILE_MARKER)).unwrap();
        let fresh_identity: ProfileIdentity = serde_json::from_slice(&fresh_marker).unwrap();
        assert_eq!(fresh_identity.application_id, DEVELOPMENT_APPLICATION_ID);
        assert_eq!(fresh_marker, original_marker);
        assert_eq!(
            std::fs::read(baseline.join("keep.txt")).unwrap(),
            b"baseline user data"
        );
        let restarted = axial_api::start_in_profile(root, None).await.unwrap();
        assert_eq!(restarted.settings.current().unwrap().revision, 0);
        restarted.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reset_releases_services_and_metadata_while_tauri_managed_facades_remain_alive() {
        let (temporary, services, lifecycle, reset) = fixture().await;
        let root = services.profile_root.clone();
        let marker = std::fs::read(root.join(PROFILE_MARKER)).unwrap();
        let baseline = temporary.path().join("baseline");
        std::fs::create_dir(&baseline).unwrap();
        std::fs::write(baseline.join("keep.txt"), b"baseline user data").unwrap();
        std::fs::write(root.join("reset.txt"), b"development data").unwrap();
        let server = Arc::downgrade(&services.server);
        let skins = Arc::downgrade(&services.skins);
        let auth = Arc::downgrade(&services.auth);
        let metadata = Arc::downgrade(services.settings.metadata());
        let pin = services.library.admit_application_root().unwrap();
        let escaped = pin.admit_native_file(&root.join("reset.txt"), 128).unwrap();
        drop(pin);
        let app = tauri::test::mock_builder()
            .manage(lifecycle.clone())
            .manage(reset.clone())
            .manage(crate::auth::NativeSignIn::new(
                services.auth.clone(),
                root.join("oauth-webview"),
            ))
            .plugin(tauri_plugin_dialog::init())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        // The real dialog plugin retains an AppHandle in managed state. Keep
        // another handle explicitly: cleanup must not depend on Tauri dropping
        // every managed facade or late IPC resolver before profile deletion.
        let retained_native = app.handle().clone();
        drop(app);
        assert!(reset.prepare(&lifecycle).await.unwrap());
        lifecycle.allow_exit();
        lifecycle.shutdown_after_event_loop().await;
        reset.quiesce_after_exit().await;
        lifecycle.release_services_after_exit().unwrap();
        lifecycle.release_services_after_exit().unwrap();
        let mut pending = reset.take_after_exit().unwrap();
        drop(lifecycle);
        drop(services);
        drop(reset);
        assert!(
            server.upgrade().is_none(),
            "native facade retained the API owner"
        );
        assert!(
            skins.upgrade().is_none(),
            "native facade retained profile media"
        );
        assert!(
            auth.upgrade().is_none(),
            "native facade retained authentication"
        );
        assert!(
            metadata.upgrade().is_none(),
            "native facade retained SQLite"
        );
        assert!(pending.try_clear().is_err());
        assert_eq!(escaped.read().unwrap(), b"development data");
        pending.try_clear().unwrap();
        assert!(!root.join("reset.txt").exists());
        assert!(!root.join("metadata.sqlite").exists());
        assert_eq!(std::fs::read(root.join(PROFILE_MARKER)).unwrap(), marker);
        assert_eq!(
            std::fs::read(baseline.join("keep.txt")).unwrap(),
            b"baseline user data"
        );
        drop(retained_native);
    }

    #[tokio::test]
    async fn reset_cancels_active_work_and_joins_it_before_scheduling_deletion() {
        let (_temporary, services, lifecycle, reset) = fixture().await;
        let (release, wait) = tokio::sync::oneshot::channel();
        let (cancelled, cancellation) = tokio::sync::oneshot::channel();
        let work = services
            .tasks
            .try_spawn((), |cancel| async move {
                cancel.cancelled().await;
                let _ = cancelled.send(());
                let _ = wait.await;
            })
            .unwrap();
        let requested = request_reset(reset.clone(), lifecycle.clone(), || {});
        cancellation.await.unwrap();
        assert!(!requested.is_finished());
        assert!(services.tasks.status().closing);
        assert!(!services.server.is_shutdown_settled());
        assert_eq!(services.library.snapshot().admission, AdmissionState::Open);
        assert!(reset.take_after_exit().is_none());
        assert!(services.profile_root.join("metadata.sqlite").exists());
        assert!(services.tasks.try_spawn((), |_| async {}).is_err());
        let cleanup_owner = lifecycle.clone();
        let cleanup = tokio::spawn(async move { cleanup_owner.shutdown_after_event_loop().await });
        let quiesce_owner = reset.clone();
        let quiesced = tokio::spawn(async move { quiesce_owner.quiesce_after_exit().await });
        tokio::task::yield_now().await;
        assert!(!quiesced.is_finished());
        assert!(reset.take_after_exit().is_none());
        release.send(()).unwrap();
        work.join().await.unwrap();
        requested.await.unwrap().unwrap();
        cleanup.await.unwrap();
        quiesced.await.unwrap();
        let mut pending = reset.take_after_exit().unwrap();
        drop(lifecycle);
        drop(services);
        pending.try_clear().unwrap();
    }

    #[tokio::test]
    async fn unexpected_loop_end_waits_for_an_accepted_worker_before_it_can_prepare() {
        let (_temporary, services, lifecycle, reset) = fixture().await;
        let root = services.profile_root.clone();
        let preparation_gate = reset.attempt.lock().await;
        let requested = request_reset(reset.clone(), lifecycle.clone(), || panic!("must not exit"));
        assert_eq!(reset.state.lock().unwrap().active_requests, 1);
        lifecycle.shutdown_after_event_loop().await;
        let quiesce_owner = reset.clone();
        let mut quiesced = tokio::spawn(async move { quiesce_owner.quiesce_after_exit().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut quiesced)
                .await
                .is_err()
        );
        assert!(reset.take_after_exit().is_none());
        drop(preparation_gate);
        assert!(requested.await.unwrap().is_err());
        quiesced.await.unwrap();
        assert!(reset.take_after_exit().is_none());
        assert!(root.join(PROFILE_MARKER).exists());
        assert!(root.join("metadata.sqlite").exists());
        assert!(
            request_reset(reset.clone(), lifecycle.clone(), || panic!(
                "closed ingress"
            ))
            .await
            .unwrap()
            .is_err()
        );
        drop(lifecycle);
        drop(services);
    }

    async fn prepared_pending() -> (tempfile::TempDir, PendingReset) {
        let (temporary, services, lifecycle, reset) = fixture().await;
        reset.prepare(&lifecycle).await.unwrap();
        let mut pending = reset.take_after_exit().unwrap();
        drop(lifecycle);
        drop(services);
        pending.pin.take();
        let session = pending
            .library
            .as_ref()
            .unwrap()
            .take_reset_session()
            .unwrap();
        let mut native = PendingRootReset::new(
            session,
            LeafName::new(PROFILE_MARKER).unwrap(),
            (*pending.marker).clone(),
        );
        native.prepare().unwrap();
        pending.pending = Some(native);
        (temporary, pending)
    }

    #[tokio::test]
    async fn accepted_reset_preserves_marker_while_durable_intent_precedes_deletion() {
        let (_temporary, mut pending) = prepared_pending().await;
        let root = pending.root.clone();
        assert!(root.join("metadata.sqlite").exists());
        assert!(root.join(".axial-reset-intent").exists());
        assert_eq!(
            std::fs::read(root.join(PROFILE_MARKER)).unwrap(),
            *pending.marker
        );
        pending.try_clear().unwrap();
        assert!(!root.join(".axial-reset-intent").exists());
        assert_eq!(
            std::fs::read(root.join(PROFILE_MARKER)).unwrap(),
            *pending.marker
        );
        let restarted = axial_api::start_in_profile(root, None).await.unwrap();
        restarted.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replaced_preserved_marker_blocks_clear_without_overwriting_then_retries() {
        let (temporary, mut pending) = prepared_pending().await;
        let root = pending.root.clone();
        let original = temporary.path().join("original-marker");
        std::fs::rename(root.join(PROFILE_MARKER), &original).unwrap();
        std::fs::write(root.join(PROFILE_MARKER), b"unexpected marker").unwrap();
        assert!(pending.try_clear().is_err());
        assert_eq!(
            std::fs::read(root.join(PROFILE_MARKER)).unwrap(),
            b"unexpected marker"
        );
        std::fs::rename(
            root.join(PROFILE_MARKER),
            temporary.path().join("preserved-marker"),
        )
        .unwrap();
        std::fs::rename(original, root.join(PROFILE_MARKER)).unwrap();
        pending.try_clear().unwrap();
        let restarted = axial_api::start_in_profile(root, None).await.unwrap();
        restarted.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn changed_profile_marker_fails_before_terminal_admission() {
        let (_temporary, services, lifecycle, reset) = fixture().await;
        let marker = services.profile_root.join(PROFILE_MARKER);
        let original = std::fs::read(&marker).unwrap();
        std::fs::write(&marker, b"{\"application_id\":\"com.mateoltd.axial\"}").unwrap();
        assert_eq!(
            reset.prepare(&lifecycle).await,
            Err(PREFLIGHT_FAILED.into())
        );
        assert!(!services.tasks.status().closing);
        assert!(!services.server.is_shutdown_settled());
        assert!(reset.take_after_exit().is_none());
        std::fs::write(marker, original).unwrap();
        lifecycle.prepare_exit(TerminalIntent::Close).await.unwrap();
        assert!(reset.take_after_exit().is_none());
    }

    #[tokio::test]
    async fn ordinary_close_does_not_keep_a_reset_pin_or_schedule_deletion() {
        let (_temporary, services, lifecycle, reset) = fixture().await;
        let root = services.profile_root.clone();
        lifecycle.prepare_exit(TerminalIntent::Close).await.unwrap();
        drop(lifecycle);
        drop(services);
        reset
            .library
            .wait_for_pins(std::time::Duration::from_millis(20))
            .await
            .unwrap();
        reset.library.try_preserve().unwrap();
        assert!(reset.take_after_exit().is_none());
        assert!(root.join(PROFILE_MARKER).exists());
        assert!(root.join("metadata.sqlite").exists());
    }

    #[tokio::test]
    async fn dropped_command_waiter_retains_one_reset_and_one_exit_request() {
        let (_temporary, services, lifecycle, reset) = fixture().await;
        let exits = Arc::new(AtomicUsize::new(0));
        let observed = exits.clone();
        let (sent, received) = tokio::sync::oneshot::channel();
        drop(request_reset(reset.clone(), lifecycle.clone(), move || {
            observed.fetch_add(1, Ordering::SeqCst);
            let _ = sent.send(());
        }));
        received.await.unwrap();
        let observed = exits.clone();
        request_reset(reset.clone(), lifecycle.clone(), move || {
            observed.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(exits.load(Ordering::SeqCst), 1);
        assert!(lifecycle.exit_allowed());
        let mut pending = reset.take_after_exit().unwrap();
        drop(lifecycle);
        drop(services);
        pending.try_clear().unwrap();
    }

    #[tokio::test]
    async fn external_library_payload_survives_development_profile_reset() {
        let (temporary, services, lifecycle, reset) = fixture().await;
        let external = temporary.path().join("external-library");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("keep.txt"), b"external library").unwrap();
        let mut switch = services.library.begin_switch().unwrap();
        switch
            .prepare_existing(&external, LibraryId::new())
            .unwrap();
        switch.commit_after_persistence().unwrap();
        reset.prepare(&lifecycle).await.unwrap();
        let mut pending = reset.take_after_exit().unwrap();
        drop(lifecycle);
        drop(services);
        pending.try_clear().unwrap();
        assert_eq!(
            std::fs::read(external.join("keep.txt")).unwrap(),
            b"external library"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replaced_profile_path_is_preserved_and_retry_uses_the_original_root() {
        let (temporary, services, lifecycle, reset) = fixture().await;
        let root = services.profile_root.clone();
        reset.prepare(&lifecycle).await.unwrap();
        let mut pending = reset.take_after_exit().unwrap();
        drop(lifecycle);
        drop(services);

        let moved = temporary.path().join("original-profile");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("keep.txt"), b"replacement user data").unwrap();
        assert_eq!(pending.try_clear(), Err(PREFLIGHT_FAILED.into()));
        assert_eq!(
            std::fs::read(root.join("keep.txt")).unwrap(),
            b"replacement user data"
        );
        assert!(moved.join("metadata.sqlite").exists());
        // Restore the test's original binding without deleting either directory.
        let replacement = temporary.path().join("preserved-replacement");
        std::fs::rename(&root, &replacement).unwrap();
        std::fs::rename(&moved, &root).unwrap();
        pending.try_clear().unwrap();
        assert!(!root.join("metadata.sqlite").exists());
        assert_eq!(
            std::fs::read(replacement.join("keep.txt")).unwrap(),
            b"replacement user data"
        );
    }
}
