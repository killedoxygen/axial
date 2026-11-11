//! Development reset: shut down through the lifecycle owner, then clear the
//! isolated development profile and restart.
use crate::{
    bootstrap::DesktopBootstrap,
    lifecycle::{DesktopLifecycle, TerminalIntent},
};
use axial_app::{
    library::{ApplicationRootPin, LibraryLifecycle},
    profile,
    tasks::retry_until_settled,
};
use axial_fs::PendingRootReset;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, State, WebviewWindow};

/// An accepted reset holds an application root pin through shutdown, so
/// preservation keeps the root session that the clear consumes.
#[derive(Clone)]
pub struct NativeReset {
    library: LibraryLifecycle,
    accepted: Arc<Mutex<Option<ApplicationRootPin>>>,
}

impl NativeReset {
    pub fn new(library: LibraryLifecycle) -> Self {
        Self {
            library,
            accepted: Arc::new(Mutex::new(None)),
        }
    }

    /// Call after every desktop and service owner has dropped. Returns whether
    /// an accepted reset cleared the profile.
    pub async fn clear_if_accepted(&self) -> bool {
        let Some(pin) = crate::lock(&self.accepted).take() else {
            return false;
        };
        drop(pin);
        let session = retry_until_settled("Development reset", || {
            std::future::ready(self.library.take_reset_session())
        })
        .await;
        let mut reset = PendingRootReset::new(session, profile::marker_name());
        retry_until_settled("Development reset", || std::future::ready(reset.try_clear())).await;
        tracing::info!("Development profile reset completed");
        true
    }
}

#[tauri::command]
pub async fn app_reset(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    lifecycle: State<'_, DesktopLifecycle>,
    reset: State<'_, NativeReset>,
) -> Result<(), String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    let lifecycle = lifecycle.inner().clone();
    let reset = reset.inner().clone();
    // Accepted shutdown survives a disconnected IPC waiter.
    tokio::spawn(async move {
        let pin = reset
            .library
            .admit_application_root()
            .map_err(|error| error.to_string())?;
        lifecycle.prepare_exit(TerminalIntent::Reset).await?;
        *crate::lock(&reset.accepted) = Some(pin);
        lifecycle.allow_exit();
        // Main clears the profile and restarts after releasing every service owner.
        app.exit(0);
        Ok(())
    })
    .await
    .map_err(|_| "Reset could not complete. Try again.".to_string())?
}
