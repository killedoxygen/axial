use crate::bootstrap::DesktopBootstrap;
use crate::discord_presence::PresenceObserver;
use crate::native_skin::NativeSkinFiles;
use axial_api::ServerHandle;
use axial_app::{skins::ProfileMedia, tasks::TaskOwner};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager, State, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tokio::sync::{Notify, watch};

const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const API_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);
const PREFERENCES_FLUSH_TIMEOUT: Duration = Duration::from_secs(15);
pub const PREFERENCES_EVENT: &str = "axial:desktop:preferences";
const PREFERENCES_INCOMPLETE: &str =
    "The interface did not finish preparing its preferences. Axial remains open; try again.";
const PREFERENCES_ACK_INVALID: &str = "The interface preference request is no longer current.";
const TERMINAL_CONFLICT: &str = "Another desktop shutdown action is already in progress.";
const SHUTDOWN_INCOMPLETE: &str =
    "Application shutdown is incomplete. Settle active work and try again.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalIntent {
    Close,
    Restart,
    Reset,
    Update,
}

type TerminalResult = Result<(), String>;
type Attempt = Arc<watch::Sender<Option<TerminalResult>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfacePreferencesPhase {
    Flush,
    Discard,
    Release,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InterfacePreferencesEvent {
    request_id: String,
    phase: InterfacePreferencesPhase,
}

struct PreferenceRequest {
    event: InterfacePreferencesEvent,
    answer: Option<bool>,
}

#[derive(Default)]
struct TerminalState {
    intent: Option<TerminalIntent>,
    active: Option<Attempt>,
    prepared: bool,
    admitted: bool,
    update_settled: bool,
    event_loop_ended: bool,
    event_loop_settled: bool,
    preference_sequence: u64,
    preferences: Option<PreferenceRequest>,
}

#[derive(Clone)]
pub struct DesktopLifecycle {
    tasks: TaskOwner,
    services: Arc<Mutex<Option<LifecycleServices>>>,
    terminal: Arc<Mutex<TerminalState>>,
    terminal_changed: Arc<Notify>,
    preferences_events: Arc<watch::Sender<Option<InterfacePreferencesEvent>>>,
    #[cfg(test)]
    preferences_disabled_for_test: bool,
    preferences_timeout: Duration,
    exit_allowed: Arc<AtomicBool>,
    presence: PresenceObserver,
    skin_files: NativeSkinFiles,
    imports: Option<crate::import::NativeImports>,
}

#[derive(Clone)]
struct LifecycleServices {
    server: Arc<ServerHandle>,
    skins: Arc<ProfileMedia>,
}

impl DesktopLifecycle {
    pub fn new(
        tasks: TaskOwner,
        server: Arc<ServerHandle>,
        presence: PresenceObserver,
        skin_files: NativeSkinFiles,
        skins: Arc<ProfileMedia>,
    ) -> Self {
        Self {
            tasks,
            services: Arc::new(Mutex::new(Some(LifecycleServices { server, skins }))),
            terminal: Arc::new(Mutex::new(TerminalState::default())),
            terminal_changed: Arc::new(Notify::new()),
            preferences_events: Arc::new(watch::channel(None).0),
            #[cfg(test)]
            preferences_disabled_for_test: false,
            preferences_timeout: PREFERENCES_FLUSH_TIMEOUT,
            exit_allowed: Arc::new(AtomicBool::new(false)),
            presence,
            skin_files,
            imports: None,
        }
    }

    fn retained_services(&self) -> Result<LifecycleServices, String> {
        self.services
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| SHUTDOWN_INCOMPLETE.to_string())
    }

    /// Tauri plugins and late IPC resolvers can retain managed facades after
    /// run_return. Release this shared payload only after actual shutdown joins;
    /// accepted effects keep their own strong captures until they return.
    pub fn release_services_after_exit(&self) -> TerminalResult {
        let state = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.event_loop_settled
            || state.active.is_some()
            || (state.admitted
                && state.intent == Some(TerminalIntent::Update)
                && !state.update_settled)
            || self.tasks.shutdown_receipt().is_none()
        {
            return Err(SHUTDOWN_INCOMPLETE.into());
        }
        let mut services = self
            .services
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if services
            .as_ref()
            .is_some_and(|services| !services.server.is_shutdown_settled())
        {
            return Err(SHUTDOWN_INCOMPLETE.into());
        }
        if let Some(imports) = &self.imports {
            imports.release_after_shutdown()?;
        }
        let retained = services.take();
        drop(services);
        drop(state);
        drop(retained);
        Ok(())
    }

    pub fn exit_allowed(&self) -> bool {
        self.exit_allowed.load(Ordering::Acquire)
    }

    /// Carries only an admitted, completed restart through ordinary event-loop
    /// exit. Main consumes this after cleanup and releases service owners before
    /// launching the successor process.
    pub fn restart_after_exit(&self) -> bool {
        let state = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.exit_allowed()
            && state.event_loop_ended
            && state.admitted
            && state.prepared
            && state.active.is_none()
            && state.intent == Some(TerminalIntent::Restart)
    }

    /// The main-owned observer emits these events using its existing AppHandle.
    /// Keeping only a channel here avoids a managed-state/AppHandle cycle.
    /// IDs are increasing `preferences-N` values without reuse in this process.
    /// Only the latest event is retained: a newer request replaces the previous
    /// native renderer seal even when its intervening release was coalesced.
    pub fn interface_preferences_events(
        &self,
    ) -> watch::Receiver<Option<InterfacePreferencesEvent>> {
        self.preferences_events.subscribe()
    }

    pub fn pending_interface_preferences(&self) -> Option<InterfacePreferencesEvent> {
        self.terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .preferences
            .as_ref()
            .map(|request| request.event.clone())
    }

    pub fn interface_preferences_delivery_failed(&self, event: &InterfacePreferencesEvent) {
        // A failed emit cannot authorize shutdown. A newer/answered request is
        // unaffected; registration races are handled by the pending command.
        let _ = self.complete_interface_preferences(&event.request_id, false);
    }

    fn complete_interface_preferences(&self, request_id: &str, saved: bool) -> TerminalResult {
        if request_id.is_empty() || request_id.len() > 32 {
            return Err(PREFERENCES_ACK_INVALID.into());
        }
        let mut state = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.event_loop_ended || state.admitted || state.active.is_none() {
            return Err(PREFERENCES_ACK_INVALID.into());
        }
        let request = state.preferences.as_mut().ok_or(PREFERENCES_ACK_INVALID)?;
        if request.event.request_id != request_id
            || request.event.phase == InterfacePreferencesPhase::Release
            || request.answer.is_some()
        {
            return Err(PREFERENCES_ACK_INVALID.into());
        }
        request.answer = Some(saved);
        self.terminal_changed.notify_waiters();
        Ok(())
    }

    #[cfg(test)]
    pub fn without_interface_preferences_for_test(mut self) -> Self {
        self.preferences_disabled_for_test = true;
        self
    }

    pub fn with_imports(mut self, imports: crate::import::NativeImports) -> Self {
        self.imports = Some(imports);
        self
    }

    fn close_native_admission(&self) {
        self.skin_files.close();
        if let Some(imports) = &self.imports {
            imports.close();
        }
    }

    /// Native terminal effects use this only after all shutdown work succeeds.
    pub fn allow_exit(&self) {
        self.exit_allowed.store(true, Ordering::Release);
    }

    pub async fn prepare_exit(&self, intent: TerminalIntent) -> TerminalResult {
        let mut receiver = {
            let mut state = self
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.event_loop_ended && (!state.admitted || state.intent != Some(intent)) {
                return Err(SHUTDOWN_INCOMPLETE.into());
            }
            if state.intent == Some(TerminalIntent::Update)
                && intent == TerminalIntent::Restart
                && state.update_settled
                && state.active.is_none()
            {
                state.intent = Some(TerminalIntent::Restart);
                state.prepared = false;
            }
            match state.intent {
                Some(active) if active != intent => return Err(TERMINAL_CONFLICT.into()),
                // Reserve before any renderer/skin await. Same-intent callers
                // join one owned attempt; competing intents cannot reuse its ACK.
                None => state.intent = Some(intent),
                Some(_) => {}
            }
            if state.prepared {
                return Ok(());
            }
            if let Some(active) = &state.active {
                active.subscribe()
            } else {
                let (sender, receiver) = watch::channel(None);
                let attempt = Arc::new(sender);
                state.active = Some(attempt.clone());
                let owner = AttemptOwner {
                    lifecycle: self.clone(),
                    attempt,
                    finished: false,
                };
                let lifecycle = self.clone();
                // Accepted shutdown owns its worker even if the IPC waiter disappears.
                tokio::spawn(async move {
                    let result = lifecycle.prepare_and_settle(intent).await;
                    owner.finish(result);
                });
                receiver
            }
        };
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| SHUTDOWN_INCOMPLETE.to_string())?;
        }
    }

    async fn prepare_and_settle(&self, intent: TerminalIntent) -> TerminalResult {
        let services = self.retained_services()?;
        let admitted = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .admitted;
        if !admitted {
            self.prepare_interface_preferences(intent).await?;
            // Preference HTTP writes and accepted skin intents must finish
            // while task admission is still open. A busy refusal remains a
            // reversible preflight, including the frontend preference seal.
            services
                .skins
                .flush_pending()
                .await
                .map_err(|error| format!("Could not finish pending skin changes: {error}"))?;
            let mut state = self
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.event_loop_ended || state.intent != Some(intent) {
                return Err(SHUTDOWN_INCOMPLETE.into());
            }
            if intent == TerminalIntent::Reset {
                // Confirmed reset discards unsent preferences and retains its
                // original cancel-and-join behavior for already accepted work.
                self.tasks.close_admission();
            } else {
                self.tasks
                    .try_close_idle()
                    .map_err(|_| busy_message(intent).to_string())?;
            }
            state.admitted = true;
            state.preferences = None;
            self.close_native_admission();
        }
        self.settle(&services, intent != TerminalIntent::Update)
            .await
    }

    async fn prepare_interface_preferences(&self, intent: TerminalIntent) -> TerminalResult {
        #[cfg(test)]
        if self.preferences_disabled_for_test {
            return Ok(());
        }
        let event = {
            let mut state = self
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.event_loop_ended {
                return Err(SHUTDOWN_INCOMPLETE.into());
            }
            state.preference_sequence = state
                .preference_sequence
                .checked_add(1)
                .ok_or(PREFERENCES_INCOMPLETE)?;
            let event = InterfacePreferencesEvent {
                request_id: format!("preferences-{}", state.preference_sequence),
                phase: if intent == TerminalIntent::Reset {
                    InterfacePreferencesPhase::Discard
                } else {
                    InterfacePreferencesPhase::Flush
                },
            };
            state.preferences = Some(PreferenceRequest {
                event: event.clone(),
                answer: None,
            });
            self.preferences_events.send_replace(Some(event.clone()));
            event
        };
        tokio::time::timeout(self.preferences_timeout, async {
            loop {
                let changed = self.terminal_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = self
                        .terminal
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if state.event_loop_ended {
                        return Err(SHUTDOWN_INCOMPLETE.to_string());
                    }
                    let request = state
                        .preferences
                        .as_ref()
                        .filter(|request| request.event.request_id == event.request_id)
                        .ok_or_else(|| PREFERENCES_INCOMPLETE.to_string())?;
                    match request.answer {
                        Some(true) => return Ok(()),
                        Some(false) => return Err(PREFERENCES_INCOMPLETE.to_string()),
                        None => {}
                    }
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| PREFERENCES_INCOMPLETE.to_string())?
    }

    /// Keeps HTTP status readable while the package-aware updater applies or fails.
    pub async fn prepare_update(&self) -> TerminalResult {
        self.prepare_exit(TerminalIntent::Update).await
    }

    /// Windows' package updater exits the process itself after spawning its
    /// installer. The adapter releases its HTTP response first, then calls this
    /// fence before native installation can make that process-exit decision.
    #[cfg(any(windows, test))]
    pub async fn prepare_update_process_exit(&self) -> TerminalResult {
        let services = self.retained_services()?;
        {
            let state = self
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.intent != Some(TerminalIntent::Update)
                || !state.prepared
                || state.update_settled
            {
                return Err("The native update has not entered its installation phase.".into());
            }
        }
        self.settle(&services, true).await
    }

    /// A panic or ambiguous native installation must not call this method.
    pub fn finish_update_settled(&self) -> TerminalResult {
        let mut state = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.intent != Some(TerminalIntent::Update) || !state.prepared {
            return Err("The native update has not entered its installation phase.".into());
        }
        state.update_settled = true;
        self.terminal_changed.notify_waiters();
        Ok(())
    }

    /// Used only when startup or the event loop has already failed. Active work
    /// receives cancellation and must still settle; timeout never authorizes exit.
    pub async fn shutdown_after_event_loop(&self) {
        let Ok(services) = self.retained_services() else {
            // Only a previously completed post-loop shutdown can release it.
            return;
        };
        self.terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .event_loop_ended = true;
        self.terminal_changed.notify_waiters();
        // A native installer owns effects outside TaskOwner. An event-loop
        // failure cannot drop that owner or turn ambiguous installation into
        // permission to exit. Register the notification before inspecting state.
        loop {
            let changed = self.terminal_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let installing = {
                let state = self
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.admitted
                    && state.intent == Some(TerminalIntent::Update)
                    && !state.update_settled
                    && (state.prepared || state.active.is_some())
            };
            if !installing {
                break;
            }
            changed.await;
        }
        self.close_native_admission();
        let presence = async {
            loop {
                match self.presence.shutdown().await {
                    Ok(()) => break,
                    Err(error) => {
                        tracing::error!(%error, "Desktop presence shutdown remains incomplete; retaining its owner");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        };
        // The API owner asks domain services to stop before joining their work.
        // Both owners survive until their actual shutdown has settled.
        tokio::join!(
            presence,
            self.skin_files.drain(),
            shutdown_server_after_failure(&services.server)
        );
        self.terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .event_loop_settled = true;
        self.terminal_changed.notify_waiters();
    }

    async fn settle(&self, services: &LifecycleServices, stop_api: bool) -> TerminalResult {
        self.tasks
            .shutdown(TASK_SHUTDOWN_TIMEOUT)
            .await
            .map_err(|_| SHUTDOWN_INCOMPLETE.to_string())?;
        self.skin_files.drain().await;
        self.presence.shutdown().await?;
        if stop_api {
            let result = tokio::time::timeout(API_SHUTDOWN_TIMEOUT, services.server.shutdown())
                .await
                .map_err(|_| {
                    "The local API has not stopped. Try the same action again.".to_string()
                })?;
            if let Err(error) = result {
                if !services.server.is_shutdown_settled() {
                    return Err(
                        "The local API did not stop cleanly. Try the same action again.".into(),
                    );
                }
                tracing::warn!(%error, "Local API shutdown settled after a worker error");
            }
        }
        Ok(())
    }

    fn finish_attempt(&self, attempt: &Attempt, result: TerminalResult) {
        let mut state = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, attempt))
        {
            state.active = None;
            state.prepared = result.is_ok();
            if !state.admitted {
                state.intent = None;
                state.update_settled = false;
                if let Some(request) = state.preferences.as_mut() {
                    request.event.phase = InterfacePreferencesPhase::Release;
                    request.answer = None;
                    self.preferences_events
                        .send_replace(Some(request.event.clone()));
                }
            }
            attempt.send_replace(Some(result));
            self.terminal_changed.notify_waiters();
        }
    }
}

#[tauri::command]
pub fn pending_interface_preferences(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    lifecycle: State<'_, DesktopLifecycle>,
) -> Result<Option<InterfacePreferencesEvent>, String> {
    crate::window::require_main_window(&window, &bootstrap)?;
    Ok(lifecycle.pending_interface_preferences())
}

#[tauri::command]
pub fn complete_interface_preferences(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    lifecycle: State<'_, DesktopLifecycle>,
    request_id: String,
    saved: bool,
) -> TerminalResult {
    crate::window::require_main_window(&window, &bootstrap)?;
    lifecycle.complete_interface_preferences(&request_id, saved)
}

/// Also used before a desktop lifecycle owner has been constructed. A transport
/// error can be reported only after the API owner proves domain settlement.
pub async fn shutdown_server_after_failure(server: &ServerHandle) {
    loop {
        match server.shutdown().await {
            Ok(()) => return,
            Err(error) if server.is_shutdown_settled() => {
                tracing::warn!(%error, "Local API shutdown settled after a worker error");
                return;
            }
            Err(error) => {
                tracing::error!(%error, "Desktop shutdown remains incomplete; retaining application services");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Setup callbacks must not return an error: Tauri would panic before retained
/// service cleanup. A native dialog stays visible until safe shutdown is possible.
pub fn report_window_startup_failure(app: AppHandle, lifecycle: DesktopLifecycle) {
    tracing::error!("Could not open the main window; retaining services until shutdown settles");
    show_window_failure(app, lifecycle.clone());
    tokio::spawn(async move {
        lifecycle.shutdown_after_event_loop().await;
        lifecycle.allow_exit();
    });
}

fn show_window_failure(app: AppHandle, lifecycle: DesktopLifecycle) {
    let message = if lifecycle.exit_allowed() {
        "The main Axial window could not open. Application work has settled. You can now close Axial and try again."
    } else {
        "The main Axial window could not open. Application work is still being settled. Axial will remain open until it is safe to close."
    };
    let handle = app.clone();
    app.dialog()
        .message(message)
        .title("Axial could not start")
        .kind(MessageDialogKind::Error)
        .show(move |_| {
            if lifecycle.exit_allowed() {
                handle.exit(1);
            } else {
                show_window_failure(handle, lifecycle);
            }
        });
}

struct AttemptOwner {
    lifecycle: DesktopLifecycle,
    attempt: Attempt,
    finished: bool,
}

impl AttemptOwner {
    fn finish(mut self, result: TerminalResult) {
        self.finished = true;
        self.lifecycle.finish_attempt(&self.attempt, result);
    }
}

impl Drop for AttemptOwner {
    fn drop(&mut self) {
        if !self.finished {
            self.lifecycle
                .finish_attempt(&self.attempt, Err(SHUTDOWN_INCOMPLETE.into()));
        }
    }
}

fn busy_message(intent: TerminalIntent) -> &'static str {
    match intent {
        TerminalIntent::Close => {
            "Close is blocked while installs, launches or other application work are active."
        }
        TerminalIntent::Restart => {
            "Restart is blocked while installs, launches or other application work are active."
        }
        TerminalIntent::Reset => "Reset is blocked while application work is active.",
        TerminalIntent::Update => "Update is blocked while application work is active.",
    }
}

pub async fn request_window_close(app: AppHandle, lifecycle: DesktopLifecycle) -> TerminalResult {
    // The native side effect also survives a dropped command response.
    tokio::spawn(async move {
        lifecycle.prepare_exit(TerminalIntent::Close).await?;
        let window = app
            .get_webview_window(crate::window::MAIN_WINDOW)
            .ok_or_else(|| "The main window is unavailable.".to_string())?;
        window
            .destroy()
            .map_err(|_| "Could not close the main window.".to_string())?;
        lifecycle.allow_exit();
        app.exit(0);
        Ok(())
    })
    .await
    .map_err(|_| SHUTDOWN_INCOMPLETE.to_string())?
}

#[tauri::command]
pub async fn window_close(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    lifecycle: State<'_, DesktopLifecycle>,
) -> TerminalResult {
    crate::window::require_main_window(&window, &bootstrap)?;
    request_window_close(app, lifecycle.inner().clone()).await
}

#[tauri::command]
pub async fn app_restart(
    app: AppHandle,
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    lifecycle: State<'_, DesktopLifecycle>,
) -> TerminalResult {
    crate::window::require_main_window(&window, &bootstrap)?;
    let lifecycle = lifecycle.inner().clone();
    tokio::spawn(async move {
        lifecycle.prepare_exit(TerminalIntent::Restart).await?;
        lifecycle.allow_exit();
        // Tauri's request_restart bypasses run_return's caller by relaunching
        // inside the Exit callback. Main must release our service owners first.
        app.exit(0);
        Ok(())
    })
    .await
    .map_err(|_| SHUTDOWN_INCOMPLETE.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, DesktopLifecycle, std::net::SocketAddr) {
        let directory = tempfile::tempdir().unwrap();
        let services = axial_api::start_in_profile(directory.path().join("rewrite"), None)
            .await
            .unwrap();
        let url = tauri::Url::parse(&services.server.bootstrap().base_url).unwrap();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], url.port().unwrap()));
        let skin_files = NativeSkinFiles::new(services.library.clone(), services.tasks.clone());
        let lifecycle = DesktopLifecycle::new(
            services.tasks,
            services.server,
            PresenceObserver::disabled_for_test(),
            skin_files,
            services.skins,
        )
        .without_interface_preferences_for_test();
        (directory, lifecycle, address)
    }

    async fn preferences_fixture() -> (tempfile::TempDir, DesktopLifecycle, std::net::SocketAddr) {
        let (directory, mut lifecycle, address) = fixture().await;
        lifecycle.preferences_disabled_for_test = false;
        (directory, lifecycle, address)
    }

    fn start_terminal(
        lifecycle: &DesktopLifecycle,
        intent: TerminalIntent,
    ) -> tokio::task::JoinHandle<TerminalResult> {
        let lifecycle = lifecycle.clone();
        tokio::spawn(async move { lifecycle.prepare_exit(intent).await })
    }

    async fn preference_event(
        events: &mut watch::Receiver<Option<InterfacePreferencesEvent>>,
    ) -> InterfacePreferencesEvent {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                events.changed().await.unwrap();
                if let Some(event) = events.borrow_and_update().clone() {
                    return event;
                }
            }
        })
        .await
        .expect("preference event")
    }

    #[tokio::test]
    async fn service_release_requires_joined_exit_and_preserves_existing_strong_captures() {
        let (_directory, lifecycle, _) = fixture().await;
        let facade = lifecycle.clone();
        let retained = lifecycle.retained_services().unwrap();
        let server = Arc::downgrade(&retained.server);
        let skins = Arc::downgrade(&retained.skins);
        assert!(lifecycle.release_services_after_exit().is_err());
        lifecycle.prepare_exit(TerminalIntent::Close).await.unwrap();
        assert!(retained.server.is_shutdown_settled());
        assert!(lifecycle.release_services_after_exit().is_err());
        lifecycle.shutdown_after_event_loop().await;
        lifecycle.release_services_after_exit().unwrap();
        assert!(facade.retained_services().is_err());
        assert!(server.upgrade().is_some());
        assert!(skins.upgrade().is_some());
        drop(retained);
        assert!(server.upgrade().is_none());
        assert!(skins.upgrade().is_none());
        facade.release_services_after_exit().unwrap();
    }

    #[tokio::test]
    async fn restart_handoff_waits_for_preferences_admission_and_event_loop_cleanup() {
        let (_directory, lifecycle, address) = preferences_fixture().await;
        let mut events = lifecycle.interface_preferences_events();
        assert!(!lifecycle.restart_after_exit());
        let restart = start_terminal(&lifecycle, TerminalIntent::Restart);
        let request = preference_event(&mut events).await;
        assert_eq!(request.phase, InterfacePreferencesPhase::Flush);
        assert!(!lifecycle.restart_after_exit());
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        lifecycle
            .complete_interface_preferences(&request.request_id, true)
            .unwrap();
        restart.await.unwrap().unwrap();
        assert!(
            lifecycle
                .retained_services()
                .unwrap()
                .server
                .is_shutdown_settled()
        );
        assert!(!lifecycle.restart_after_exit());
        lifecycle.allow_exit();
        assert!(!lifecycle.restart_after_exit());
        lifecycle.shutdown_after_event_loop().await;
        assert!(lifecycle.restart_after_exit());
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn busy_restart_cannot_turn_later_failure_cleanup_into_a_restart() {
        let (_directory, lifecycle, address) = fixture().await;
        let (finish, wait) = tokio::sync::oneshot::channel();
        let work = lifecycle
            .tasks
            .try_spawn((), |_| async move {
                let _ = wait.await;
            })
            .unwrap();
        assert_eq!(
            lifecycle.prepare_exit(TerminalIntent::Restart).await,
            Err(busy_message(TerminalIntent::Restart).into())
        );
        assert!(!lifecycle.restart_after_exit());
        assert!(!lifecycle.exit_allowed());
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        finish.send(()).unwrap();
        work.join().await.unwrap();
        lifecycle.shutdown_after_event_loop().await;
        lifecycle.allow_exit();
        assert!(!lifecycle.restart_after_exit());
    }

    #[tokio::test]
    async fn close_reset_and_settled_update_do_not_request_a_restart() {
        for intent in [
            TerminalIntent::Close,
            TerminalIntent::Reset,
            TerminalIntent::Update,
        ] {
            let (_directory, lifecycle, _) = fixture().await;
            lifecycle.prepare_exit(intent).await.unwrap();
            if intent == TerminalIntent::Update {
                lifecycle.finish_update_settled().unwrap();
            }
            lifecycle.allow_exit();
            lifecycle.shutdown_after_event_loop().await;
            assert!(!lifecycle.restart_after_exit(), "{intent:?}");
        }
    }

    #[tokio::test]
    async fn preference_flush_keeps_admission_open_until_delayed_draft_commit_acknowledgement() {
        let (_directory, lifecycle, address) = preferences_fixture().await;
        let mut events = lifecycle.interface_preferences_events();
        let mut closing = start_terminal(&lifecycle, TerminalIntent::Close);
        let request = preference_event(&mut events).await;
        assert_eq!(request.phase, InterfacePreferencesPhase::Flush);
        assert_eq!(
            lifecycle.pending_interface_preferences(),
            Some(request.clone())
        );
        assert!(!lifecycle.tasks.status().closing);
        let (commit, wait) = tokio::sync::oneshot::channel();
        let committed = Arc::new(AtomicBool::new(false));
        let observed = committed.clone();
        // A draft first becomes accepted application work after the native
        // request. Its commit must still be admitted while native waits.
        let writing = lifecycle
            .tasks
            .try_spawn((), |_| async move {
                wait.await.unwrap();
                observed.store(true, Ordering::Release);
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut closing)
                .await
                .is_err()
        );
        assert!(!committed.load(Ordering::Acquire));
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        commit.send(()).unwrap();
        writing.join().await.unwrap();
        lifecycle
            .complete_interface_preferences(&request.request_id, true)
            .unwrap();
        closing.await.unwrap().unwrap();
        assert!(committed.load(Ordering::Acquire));
        assert!(lifecycle.tasks.status().closing);
        assert!(lifecycle.pending_interface_preferences().is_none());
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn refused_preference_preflight_releases_only_its_request_for_every_terminal_intent() {
        for intent in [
            TerminalIntent::Close,
            TerminalIntent::Restart,
            TerminalIntent::Update,
            TerminalIntent::Reset,
        ] {
            let (_directory, lifecycle, address) = preferences_fixture().await;
            let mut events = lifecycle.interface_preferences_events();
            let attempt = start_terminal(&lifecycle, intent);
            let request = preference_event(&mut events).await;
            assert!(
                lifecycle
                    .complete_interface_preferences("wrong-request", true)
                    .is_err()
            );
            assert!(
                lifecycle
                    .complete_interface_preferences(&"x".repeat(33), true)
                    .is_err()
            );
            assert_eq!(
                lifecycle.pending_interface_preferences(),
                Some(request.clone())
            );
            lifecycle
                .complete_interface_preferences(&request.request_id, false)
                .unwrap();
            assert!(attempt.await.unwrap().is_err());
            let released = preference_event(&mut events).await;
            assert_eq!(released.request_id, request.request_id);
            assert_eq!(released.phase, InterfacePreferencesPhase::Release);
            assert!(
                lifecycle
                    .complete_interface_preferences(&request.request_id, true)
                    .is_err()
            );
            assert!(!lifecycle.tasks.status().closing);
            assert!(lifecycle.terminal.lock().unwrap().intent.is_none());
            assert!(!lifecycle.restart_after_exit());
            assert!(tokio::net::TcpStream::connect(address).await.is_ok());
            lifecycle.shutdown_after_event_loop().await;
        }
    }

    #[tokio::test]
    async fn concurrent_terminal_callers_share_one_preflight_and_dropped_waiter_does_not_cancel_it()
    {
        let (_directory, lifecycle, _) = preferences_fixture().await;
        let mut events = lifecycle.interface_preferences_events();
        let first = start_terminal(&lifecycle, TerminalIntent::Close);
        let request = preference_event(&mut events).await;
        let second = start_terminal(&lifecycle, TerminalIntent::Close);
        assert_eq!(
            lifecycle.prepare_exit(TerminalIntent::Restart).await,
            Err(TERMINAL_CONFLICT.into())
        );
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(
            lifecycle.pending_interface_preferences(),
            Some(request.clone())
        );
        lifecycle
            .complete_interface_preferences(&request.request_id, true)
            .unwrap();
        assert!(
            lifecycle
                .complete_interface_preferences(&request.request_id, true)
                .is_err()
        );
        second.await.unwrap().unwrap();
        assert_eq!(lifecycle.terminal.lock().unwrap().preference_sequence, 1);
        assert_eq!(
            events.borrow().as_ref().unwrap().phase,
            InterfacePreferencesPhase::Flush
        );
    }

    #[tokio::test]
    async fn busy_refusal_releases_seal_and_retry_requires_a_fresh_acknowledgement() {
        let (_directory, lifecycle, address) = preferences_fixture().await;
        let (finish, wait) = tokio::sync::oneshot::channel();
        let work = lifecycle
            .tasks
            .try_spawn((), |_| async move {
                let _ = wait.await;
            })
            .unwrap();
        let mut events = lifecycle.interface_preferences_events();
        let first = start_terminal(&lifecycle, TerminalIntent::Close);
        let original = preference_event(&mut events).await;
        lifecycle
            .complete_interface_preferences(&original.request_id, true)
            .unwrap();
        assert_eq!(
            first.await.unwrap(),
            Err(busy_message(TerminalIntent::Close).into())
        );
        assert_eq!(
            preference_event(&mut events).await.phase,
            InterfacePreferencesPhase::Release
        );
        assert!(!lifecycle.tasks.status().closing);
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        finish.send(()).unwrap();
        work.join().await.unwrap();
        let second = start_terminal(&lifecycle, TerminalIntent::Close);
        let fresh = preference_event(&mut events).await;
        assert_ne!(fresh.request_id, original.request_id);
        assert!(
            lifecycle
                .complete_interface_preferences(&original.request_id, true)
                .is_err()
        );
        lifecycle
            .complete_interface_preferences(&fresh.request_id, true)
            .unwrap();
        second.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn coalesced_release_and_retry_exposes_only_the_newer_request() {
        let (_directory, lifecycle, _) = preferences_fixture().await;
        let mut events = lifecycle.interface_preferences_events();
        let mut delayed = lifecycle.interface_preferences_events();
        let first = start_terminal(&lifecycle, TerminalIntent::Close);
        let original = preference_event(&mut events).await;
        assert_eq!(preference_event(&mut delayed).await, original);
        lifecycle
            .complete_interface_preferences(&original.request_id, false)
            .unwrap();
        assert!(first.await.unwrap().is_err());
        assert_eq!(
            preference_event(&mut events).await.phase,
            InterfacePreferencesPhase::Release
        );

        // This receiver has not consumed release(1). A newer request replaces
        // it, so the renderer must replace its native-only seal as well.
        let second = start_terminal(&lifecycle, TerminalIntent::Close);
        let fresh = preference_event(&mut events).await;
        assert_eq!(original.request_id, "preferences-1");
        assert_eq!(fresh.request_id, "preferences-2");
        assert_eq!(preference_event(&mut delayed).await, fresh);
        assert_eq!(
            lifecycle.pending_interface_preferences(),
            Some(fresh.clone())
        );
        assert!(
            lifecycle
                .complete_interface_preferences(&original.request_id, true)
                .is_err()
        );
        lifecycle
            .complete_interface_preferences(&fresh.request_id, true)
            .unwrap();
        second.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn preference_timeout_and_delivery_failure_leave_the_app_open_and_reject_late_ack() {
        let (_directory, mut lifecycle, address) = preferences_fixture().await;
        lifecycle.preferences_timeout = Duration::from_millis(20);
        let mut events = lifecycle.interface_preferences_events();
        let first = start_terminal(&lifecycle, TerminalIntent::Close);
        let request = preference_event(&mut events).await;
        assert!(first.await.unwrap().is_err());
        assert_eq!(
            preference_event(&mut events).await.phase,
            InterfacePreferencesPhase::Release
        );
        assert!(
            lifecycle
                .complete_interface_preferences(&request.request_id, true)
                .is_err()
        );
        assert!(!lifecycle.tasks.status().closing);
        let second = start_terminal(&lifecycle, TerminalIntent::Close);
        let request = preference_event(&mut events).await;
        lifecycle.interface_preferences_delivery_failed(&request);
        assert!(second.await.unwrap().is_err());
        assert!(!lifecycle.tasks.status().closing);
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        lifecycle.shutdown_after_event_loop().await;
    }

    #[tokio::test]
    async fn reset_discards_unsent_drafts_but_still_cancels_and_joins_accepted_work() {
        let (_directory, lifecycle, _) = preferences_fixture().await;
        let work = lifecycle
            .tasks
            .try_spawn((), |context| async move {
                context.cancelled().await;
            })
            .unwrap();
        let mut events = lifecycle.interface_preferences_events();
        let reset = start_terminal(&lifecycle, TerminalIntent::Reset);
        let request = preference_event(&mut events).await;
        assert_eq!(request.phase, InterfacePreferencesPhase::Discard);
        assert!(!lifecycle.tasks.status().closing);
        lifecycle
            .complete_interface_preferences(&request.request_id, true)
            .unwrap();
        reset.await.unwrap().unwrap();
        work.join().await.unwrap();
        assert!(lifecycle.tasks.shutdown_receipt().is_some());
    }

    #[tokio::test]
    async fn update_flushes_before_installation_fence_and_restart_reuses_its_held_seal() {
        let (_directory, lifecycle, address) = preferences_fixture().await;
        let mut events = lifecycle.interface_preferences_events();
        let update = start_terminal(&lifecycle, TerminalIntent::Update);
        let request = preference_event(&mut events).await;
        assert_eq!(request.phase, InterfacePreferencesPhase::Flush);
        assert!(lifecycle.prepare_update_process_exit().await.is_err());
        lifecycle
            .complete_interface_preferences(&request.request_id, true)
            .unwrap();
        update.await.unwrap().unwrap();
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        lifecycle.finish_update_settled().unwrap();
        lifecycle
            .prepare_exit(TerminalIntent::Restart)
            .await
            .unwrap();
        assert_eq!(lifecycle.terminal.lock().unwrap().preference_sequence, 1);
        assert_eq!(
            events.borrow().as_ref().unwrap().phase,
            InterfacePreferencesPhase::Flush
        );
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn failed_event_loop_teardown_does_not_wait_for_dead_renderer_or_unstarted_installer() {
        let (_directory, lifecycle, address) = preferences_fixture().await;
        let mut events = lifecycle.interface_preferences_events();
        let update = start_terminal(&lifecycle, TerminalIntent::Update);
        let request = preference_event(&mut events).await;
        tokio::time::timeout(
            Duration::from_secs(2),
            lifecycle.shutdown_after_event_loop(),
        )
        .await
        .unwrap();
        assert!(update.await.unwrap().is_err());
        assert!(
            lifecycle
                .complete_interface_preferences(&request.request_id, true)
                .is_err()
        );
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn busy_close_preserves_api_and_admission_then_retry_settles() {
        let (_directory, lifecycle, address) = fixture().await;
        let (release, wait) = tokio::sync::oneshot::channel();
        let work = lifecycle
            .tasks
            .try_spawn((), |_| async move {
                let _ = wait.await;
            })
            .unwrap();
        assert_eq!(
            lifecycle.prepare_exit(TerminalIntent::Close).await,
            Err(busy_message(TerminalIntent::Close).into())
        );
        assert!(!lifecycle.tasks.status().closing);
        assert!(!lifecycle.exit_allowed());
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        release.send(()).unwrap();
        work.join().await.unwrap();
        lifecycle.prepare_exit(TerminalIntent::Close).await.unwrap();
        assert!(lifecycle.tasks.status().closing);
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        // Preparing shutdown alone cannot grant a native exit side effect.
        assert!(!lifecycle.exit_allowed());
    }

    #[tokio::test]
    async fn repeated_close_joins_one_terminal_intent_and_rejects_restart() {
        let (_directory, lifecycle, _) = fixture().await;
        let (first, second) = tokio::join!(
            lifecycle.prepare_exit(TerminalIntent::Close),
            lifecycle.prepare_exit(TerminalIntent::Close),
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(
            lifecycle.prepare_exit(TerminalIntent::Restart).await,
            Err(TERMINAL_CONFLICT.into())
        );
        assert!(!lifecycle.exit_allowed());
    }

    #[tokio::test]
    async fn update_keeps_status_available_until_native_installation_has_settled() {
        let (_directory, lifecycle, address) = fixture().await;
        assert!(lifecycle.finish_update_settled().is_err());
        lifecycle.prepare_update().await.unwrap();
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        assert_eq!(
            lifecycle.prepare_exit(TerminalIntent::Restart).await,
            Err(TERMINAL_CONFLICT.into())
        );
        assert!(matches!(
            lifecycle.tasks.try_spawn((), |_| async {}),
            Err(axial_app::tasks::SpawnError::Closed)
        ));
        lifecycle.finish_update_settled().unwrap();
        lifecycle
            .prepare_exit(TerminalIntent::Restart)
            .await
            .unwrap();
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn interrupted_failure_cleanup_retains_running_work_for_the_next_waiter() {
        let (_directory, lifecycle, address) = fixture().await;
        let (release, wait) = tokio::sync::oneshot::channel();
        let work = lifecycle
            .tasks
            .try_spawn((), |_| async move {
                let _ = wait.await;
            })
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                lifecycle.shutdown_after_event_loop()
            )
            .await
            .is_err()
        );
        assert!(!lifecycle.tasks.status().is_idle());
        assert!(lifecycle.release_services_after_exit().is_err());
        assert!(
            !lifecycle
                .retained_services()
                .unwrap()
                .server
                .is_shutdown_settled()
        );
        assert!(!lifecycle.exit_allowed());
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        release.send(()).unwrap();
        work.join().await.unwrap();
        lifecycle.shutdown_after_event_loop().await;
        assert!(
            lifecycle
                .retained_services()
                .unwrap()
                .server
                .is_shutdown_settled()
        );
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn windows_update_process_exit_requires_preparation_and_joins_the_api() {
        let (_directory, lifecycle, address) = fixture().await;
        assert!(lifecycle.prepare_update_process_exit().await.is_err());
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        lifecycle.prepare_update().await.unwrap();
        lifecycle.prepare_update_process_exit().await.unwrap();
        assert!(
            lifecycle
                .retained_services()
                .unwrap()
                .server
                .is_shutdown_settled()
        );
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        assert!(!lifecycle.exit_allowed());
        lifecycle.finish_update_settled().unwrap();
        assert!(lifecycle.prepare_update_process_exit().await.is_err());
    }

    #[tokio::test]
    async fn event_loop_failure_keeps_unsettled_native_installation_owned_until_notified() {
        let (_directory, lifecycle, address) = fixture().await;
        lifecycle.prepare_update().await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                lifecycle.shutdown_after_event_loop(),
            )
            .await
            .is_err()
        );
        assert!(
            !lifecycle
                .retained_services()
                .unwrap()
                .server
                .is_shutdown_settled()
        );
        assert!(!lifecycle.exit_allowed());
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        let cleanup_owner = lifecycle.clone();
        let cleanup = tokio::spawn(async move { cleanup_owner.shutdown_after_event_loop().await });
        tokio::task::yield_now().await;
        assert!(!cleanup.is_finished());
        assert!(lifecycle.release_services_after_exit().is_err());
        lifecycle.finish_update_settled().unwrap();
        tokio::time::timeout(Duration::from_secs(2), cleanup)
            .await
            .unwrap()
            .unwrap();
        assert!(
            lifecycle
                .retained_services()
                .unwrap()
                .server
                .is_shutdown_settled()
        );
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn failed_event_loop_prevents_a_new_native_installation() {
        let (_directory, lifecycle, _) = fixture().await;
        lifecycle.shutdown_after_event_loop().await;
        assert_eq!(
            lifecycle.prepare_update().await,
            Err(SHUTDOWN_INCOMPLETE.into())
        );
        assert!(lifecycle.finish_update_settled().is_err());
    }
}
