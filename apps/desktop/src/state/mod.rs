use crate::native_skin::NativeSkinDropCoordinator;
use axial_api::app::{ApiServerShutdownError, ServerHandle};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const TERMINAL_STATE_LOCK_INVARIANT: &str =
    "desktop terminal-state lock poisoned; action ownership may be inconsistent";
const EVENT_TASK_LOCK_INVARIANT: &str =
    "desktop event task lock poisoned; stream ownership may be inconsistent";

#[derive(Clone)]
pub struct DesktopState {
    version: String,
    terminal: TerminalActionCoordinator,
    install_events: EventTaskCoordinator,
    loader_install_events: EventTaskCoordinator,
    launch_events: EventTaskCoordinator,
    native_skin_drop: NativeSkinDropCoordinator,
}

impl DesktopState {
    pub fn new(version: String) -> Self {
        let lifecycle_gate = Arc::new(Mutex::new(()));
        let native_skin_drop = NativeSkinDropCoordinator::new(Arc::clone(&lifecycle_gate));
        let terminal = TerminalActionCoordinator::new(
            lifecycle_gate,
            native_skin_drop.clone(),
        );
        Self {
            version,
            terminal,
            install_events: EventTaskCoordinator::new(),
            loader_install_events: EventTaskCoordinator::new(),
            launch_events: EventTaskCoordinator::new(),
            native_skin_drop,
        }
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn terminal_is_claimed(&self, intent: TerminalIntent) -> bool {
        self.terminal.is_claimed(intent)
    }

    pub fn begin_terminal(
        &self,
        intent: TerminalIntent,
    ) -> Result<TerminalAttemptStart, TerminalIntentConflict> {
        self.terminal.begin(intent)
    }

    pub fn install_events(&self) -> &EventTaskCoordinator {
        &self.install_events
    }

    pub fn loader_install_events(&self) -> &EventTaskCoordinator {
        &self.loader_install_events
    }

    pub fn launch_events(&self) -> &EventTaskCoordinator {
        &self.launch_events
    }

    pub(crate) fn native_skin_drop(&self) -> &NativeSkinDropCoordinator {
        &self.native_skin_drop
    }
}

#[derive(Clone)]
pub struct EventTaskCoordinator {
    shared: Arc<Mutex<EventTaskState>>,
}

struct EventTaskState {
    next_owner_id: u64,
    active: HashMap<String, EventTaskEntry>,
}

struct EventTaskEntry {
    owner_id: u64,
    cancel: watch::Sender<bool>,
    emission_gate: Arc<Mutex<()>>,
}

pub struct EventTaskOwner {
    coordinator: EventTaskCoordinator,
    stream_id: String,
    owner_id: u64,
    cancel: watch::Receiver<bool>,
    emission_gate: Arc<Mutex<()>>,
}

impl EventTaskCoordinator {
    fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(EventTaskState {
                next_owner_id: 0,
                active: HashMap::new(),
            })),
        }
    }

    pub fn replace(&self, stream_id: String) -> EventTaskOwner {
        let (cancel, cancel_receiver) = watch::channel(false);
        let (owner_id, emission_gate, previous_cancel) = {
            let mut state = self.shared.lock().expect(EVENT_TASK_LOCK_INVARIANT);
            state.next_owner_id = state
                .next_owner_id
                .checked_add(1)
                .expect("desktop event owner id overflowed");
            let owner_id = state.next_owner_id;
            let previous = state.active.remove(&stream_id);
            let emission_gate = previous
                .as_ref()
                .map(|entry| entry.emission_gate.clone())
                .unwrap_or_else(|| Arc::new(Mutex::new(())));
            let previous_cancel = previous.map(|entry| entry.cancel);
            state.active.insert(
                stream_id.clone(),
                EventTaskEntry {
                    owner_id,
                    cancel,
                    emission_gate: emission_gate.clone(),
                },
            );
            (owner_id, emission_gate, previous_cancel)
        };

        if let Some(previous_cancel) = previous_cancel {
            previous_cancel.send_replace(true);
        }
        {
            let _emission = emission_gate.lock().expect(EVENT_TASK_LOCK_INVARIANT);
        }

        EventTaskOwner {
            coordinator: self.clone(),
            stream_id,
            owner_id,
            cancel: cancel_receiver,
            emission_gate,
        }
    }

    fn is_current(&self, stream_id: &str, owner_id: u64) -> bool {
        self.shared
            .lock()
            .expect(EVENT_TASK_LOCK_INVARIANT)
            .active
            .get(stream_id)
            .is_some_and(|entry| entry.owner_id == owner_id)
    }

    fn retire(&self, stream_id: &str, owner_id: u64) {
        let mut state = self.shared.lock().expect(EVENT_TASK_LOCK_INVARIANT);
        if state
            .active
            .get(stream_id)
            .is_some_and(|entry| entry.owner_id == owner_id)
        {
            state.active.remove(stream_id);
        }
    }
}

impl EventTaskOwner {
    pub async fn cancelled(&mut self) {
        if *self.cancel.borrow() {
            return;
        }
        let _ = self.cancel.changed().await;
    }

    pub fn emit_if_current<E>(&self, emit: impl FnOnce() -> Result<(), E>) -> Result<bool, E> {
        let _emission = self.emission_gate.lock().expect(EVENT_TASK_LOCK_INVARIANT);
        if !self.coordinator.is_current(&self.stream_id, self.owner_id) {
            return Ok(false);
        }
        match emit() {
            Ok(()) => Ok(true),
            Err(error) => {
                self.coordinator.retire(&self.stream_id, self.owner_id);
                Err(error)
            }
        }
    }
}

impl Drop for EventTaskOwner {
    fn drop(&mut self) {
        self.coordinator.retire(&self.stream_id, self.owner_id);
    }
}

#[derive(Clone)]
pub struct ApiRuntimeState {
    server: Arc<ServerHandle>,
}

impl ApiRuntimeState {
    pub fn new(server: ServerHandle) -> Self {
        Self {
            server: Arc::new(server),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr
    }

    pub async fn wait(&self) -> Result<(), ApiServerShutdownError> {
        self.server.wait().await
    }

    pub async fn shutdown(&self) -> Result<(), ApiServerShutdownError> {
        self.server.shutdown().await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalIntent {
    Restart,
    Close,
    Reset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalFailure {
    ApiShutdown,
    AppShutdown,
    ResetPreflight,
    ResetDeletion,
    WindowClose,
    OwnerStopped,
}

pub type TerminalResult = Result<(), TerminalFailure>;
type TerminalAttemptChannel = Arc<watch::Sender<Option<TerminalResult>>>;

#[derive(Clone)]
pub struct TerminalActionCoordinator {
    shared: Arc<Mutex<TerminalActionState>>,
    lifecycle_gate: Arc<Mutex<()>>,
    native_skin_drop: NativeSkinDropCoordinator,
}

struct TerminalActionState {
    intent: Option<TerminalIntent>,
    active: Option<TerminalAttemptChannel>,
    completed: Option<TerminalResult>,
}

pub struct TerminalAttempt {
    result: watch::Receiver<Option<TerminalResult>>,
}

pub struct TerminalAttemptOwner {
    coordinator: TerminalActionCoordinator,
    intent: TerminalIntent,
    attempt: TerminalAttemptChannel,
    finished: bool,
}

pub struct TerminalAttemptStart {
    pub attempt: TerminalAttempt,
    pub owner: Option<TerminalAttemptOwner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalIntentConflict {
    pub active: TerminalIntent,
    pub requested: TerminalIntent,
}

impl TerminalActionCoordinator {
    fn new(
        lifecycle_gate: Arc<Mutex<()>>,
        native_skin_drop: NativeSkinDropCoordinator,
    ) -> Self {
        Self {
            shared: Arc::new(Mutex::new(TerminalActionState {
                intent: None,
                active: None,
                completed: None,
            })),
            lifecycle_gate,
            native_skin_drop,
        }
    }

    fn begin(
        &self,
        intent: TerminalIntent,
    ) -> Result<TerminalAttemptStart, TerminalIntentConflict> {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .expect(TERMINAL_STATE_LOCK_INVARIANT);
        let mut state = self.shared.lock().expect(TERMINAL_STATE_LOCK_INVARIANT);
        match state.intent {
            Some(active) if active != intent => {
                return Err(TerminalIntentConflict {
                    active,
                    requested: intent,
                });
            }
            None => {
                state.intent = Some(intent);
                self.native_skin_drop
                    .fence_for_terminal_while_lifecycle_locked();
            }
            Some(_) => {}
        }

        if let Some(active) = state.active.as_ref() {
            return Ok(TerminalAttemptStart {
                attempt: TerminalAttempt {
                    result: active.subscribe(),
                },
                owner: None,
            });
        }

        if matches!(state.completed, Some(Ok(()))) {
            let (_, result) = watch::channel(Some(Ok(())));
            return Ok(TerminalAttemptStart {
                attempt: TerminalAttempt { result },
                owner: None,
            });
        }

        let (attempt, result) = watch::channel(None);
        let attempt = Arc::new(attempt);
        state.active = Some(attempt.clone());
        state.completed = None;
        Ok(TerminalAttemptStart {
            attempt: TerminalAttempt { result },
            owner: Some(TerminalAttemptOwner {
                coordinator: self.clone(),
                intent,
                attempt,
                finished: false,
            }),
        })
    }

    fn is_claimed(&self, intent: TerminalIntent) -> bool {
        self.shared
            .lock()
            .expect(TERMINAL_STATE_LOCK_INVARIANT)
            .intent
            == Some(intent)
    }

    fn finish(
        &self,
        intent: TerminalIntent,
        attempt: &TerminalAttemptChannel,
        result: TerminalResult,
    ) {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .expect(TERMINAL_STATE_LOCK_INVARIANT);
        let mut state = self.shared.lock().expect(TERMINAL_STATE_LOCK_INVARIANT);
        if state.intent != Some(intent)
            || !state
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, attempt))
        {
            return;
        }
        state.active = None;
        if result == Err(TerminalFailure::ResetPreflight) {
            state.completed = None;
            state.intent = None;
            self.native_skin_drop
                .reopen_after_preflight_failure_while_lifecycle_locked();
        } else {
            state.completed = Some(result);
        }
        attempt.send_replace(Some(result));
    }
}

impl TerminalAttempt {
    pub async fn wait(mut self) -> TerminalResult {
        loop {
            if let Some(result) = *self.result.borrow_and_update() {
                return result;
            }
            if self.result.changed().await.is_err() {
                return Err(TerminalFailure::OwnerStopped);
            }
        }
    }
}

impl TerminalAttemptOwner {
    pub async fn wait_for_ingress_drain(&self) {
        self.coordinator
            .native_skin_drop
            .wait_for_ingress_drain()
            .await;
    }

    pub fn finish(mut self, result: TerminalResult) {
        self.finished = true;
        self.coordinator.finish(self.intent, &self.attempt, result);
    }
}

impl Drop for TerminalAttemptOwner {
    fn drop(&mut self) {
        if !self.finished {
            self.coordinator.finish(
                self.intent,
                &self.attempt,
                Err(TerminalFailure::OwnerStopped),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DesktopState, EventTaskCoordinator, TerminalActionCoordinator, TerminalFailure,
        TerminalIntent,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    fn terminal_coordinator() -> TerminalActionCoordinator {
        DesktopState::new("test".to_string()).terminal
    }

    #[tokio::test]
    async fn replacing_event_owner_cancels_the_previous_owner() {
        let coordinator = EventTaskCoordinator::new();
        let mut first = coordinator.replace("session".to_string());
        let second = coordinator.replace("session".to_string());

        first.cancelled().await;
        assert!(coordinator.is_current("session", second.owner_id));
        assert!(!coordinator.is_current("session", first.owner_id));
    }

    #[test]
    fn stale_event_owner_cannot_emit_or_retire_replacement() {
        let coordinator = EventTaskCoordinator::new();
        let first = coordinator.replace("session".to_string());
        let second = coordinator.replace("session".to_string());
        let emitted = AtomicUsize::new(0);

        assert_eq!(
            first.emit_if_current(|| {
                emitted.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            }),
            Ok(false)
        );
        drop(first);
        assert!(coordinator.is_current("session", second.owner_id));
        assert_eq!(
            second.emit_if_current(|| {
                emitted.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            }),
            Ok(true)
        );
        assert_eq!(emitted.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn replacing_event_owner_waits_for_in_flight_emission() {
        let coordinator = EventTaskCoordinator::new();
        let first = coordinator.replace("session".to_string());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let emitter = std::thread::spawn(move || {
            first
                .emit_if_current(|| {
                    entered_tx.send(()).expect("signal active emission");
                    release_rx.recv().expect("release active emission");
                    Ok::<(), ()>(())
                })
                .expect("active emission")
        });
        entered_rx.recv().expect("emission entered");

        let replacement_coordinator = coordinator.clone();
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let replacement = std::thread::spawn(move || {
            let owner = replacement_coordinator.replace("session".to_string());
            assert!(replacement_tx.send(owner).is_ok(), "return replacement");
        });
        assert!(
            replacement_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );

        release_tx.send(()).expect("release emission");
        assert!(emitter.join().expect("join emitter"));
        let second = replacement_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("replacement after emission");
        replacement.join().expect("join replacement");
        assert!(coordinator.is_current("session", second.owner_id));
    }

    #[test]
    fn event_emit_failure_retires_exact_owner() {
        let coordinator = EventTaskCoordinator::new();
        let owner = coordinator.replace("session".to_string());

        assert_eq!(
            owner.emit_if_current(|| Err::<(), _>("emit failed")),
            Err("emit failed")
        );
        assert!(!coordinator.is_current("session", owner.owner_id));
    }

    #[test]
    fn event_emission_gates_are_independent_per_stream() {
        let coordinator = EventTaskCoordinator::new();
        let first = coordinator.replace("first".to_string());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let emitter = std::thread::spawn(move || {
            first
                .emit_if_current(|| {
                    entered_tx.send(()).expect("signal first emission");
                    release_rx.recv().expect("release first emission");
                    Ok::<(), ()>(())
                })
                .expect("first emission")
        });
        entered_rx.recv().expect("first emission entered");

        let other = coordinator.replace("other".to_string());
        assert!(coordinator.is_current("other", other.owner_id));

        release_tx.send(()).expect("release first emission");
        assert!(emitter.join().expect("join first emitter"));
    }

    #[test]
    fn dropping_current_event_owner_retires_it() {
        let coordinator = EventTaskCoordinator::new();
        let owner = coordinator.replace("session".to_string());
        let owner_id = owner.owner_id;

        drop(owner);

        assert!(!coordinator.is_current("session", owner_id));
    }

    #[tokio::test]
    async fn same_intent_joins_one_active_attempt() {
        let coordinator = terminal_coordinator();
        let first = coordinator
            .begin(TerminalIntent::Reset)
            .expect("claim reset");
        let joined = coordinator
            .begin(TerminalIntent::Reset)
            .expect("join reset");
        assert!(joined.owner.is_none());

        first.owner.expect("first owner").finish(Ok(()));

        assert_eq!(first.attempt.wait().await, Ok(()));
        assert_eq!(joined.attempt.wait().await, Ok(()));
    }

    #[test]
    fn conflicting_intent_is_rejected_after_claim() {
        let coordinator = terminal_coordinator();
        let reset = coordinator
            .begin(TerminalIntent::Reset)
            .expect("claim reset");

        let conflict = match coordinator.begin(TerminalIntent::Restart) {
            Ok(_) => panic!("restart must not displace reset"),
            Err(conflict) => conflict,
        };

        assert_eq!(conflict.active, TerminalIntent::Reset);
        assert_eq!(conflict.requested, TerminalIntent::Restart);
        drop(reset);
    }

    #[tokio::test]
    async fn failed_attempt_allows_only_same_intent_retry() {
        let coordinator = terminal_coordinator();
        let first = coordinator
            .begin(TerminalIntent::Reset)
            .expect("claim reset");
        first
            .owner
            .expect("first owner")
            .finish(Err(TerminalFailure::ResetDeletion));
        assert_eq!(
            first.attempt.wait().await,
            Err(TerminalFailure::ResetDeletion)
        );

        assert!(coordinator.begin(TerminalIntent::Close).is_err());
        let retry = coordinator
            .begin(TerminalIntent::Reset)
            .expect("retry reset");
        assert!(retry.owner.is_some());
        retry.owner.expect("retry owner").finish(Ok(()));
        assert_eq!(retry.attempt.wait().await, Ok(()));
    }

    #[tokio::test]
    async fn reset_preflight_failure_releases_every_terminal_intent_after_reporting() {
        for next_intent in [
            TerminalIntent::Reset,
            TerminalIntent::Restart,
            TerminalIntent::Close,
        ] {
            let coordinator = terminal_coordinator();
            let first = coordinator
                .begin(TerminalIntent::Reset)
                .expect("claim reset");
            let joined = coordinator
                .begin(TerminalIntent::Reset)
                .expect("join reset preflight");
            assert!(joined.owner.is_none());
            first
                .owner
                .expect("reset owner")
                .finish(Err(TerminalFailure::ResetPreflight));
            assert_eq!(
                first.attempt.wait().await,
                Err(TerminalFailure::ResetPreflight)
            );
            assert_eq!(
                joined.attempt.wait().await,
                Err(TerminalFailure::ResetPreflight)
            );

            let next = coordinator
                .begin(next_intent)
                .expect("preflight failure releases terminal claim");
            assert!(next.owner.is_some());
            next.owner.expect("next owner").finish(Ok(()));
            assert_eq!(next.attempt.wait().await, Ok(()));
        }
    }

    #[tokio::test]
    async fn dropping_a_waiter_does_not_abandon_the_owner() {
        let coordinator = terminal_coordinator();
        let first = coordinator
            .begin(TerminalIntent::Restart)
            .expect("claim restart");
        let owner = first.owner.expect("restart owner");
        drop(first.attempt);

        let joined = coordinator
            .begin(TerminalIntent::Restart)
            .expect("join restart");
        owner.finish(Ok(()));

        assert_eq!(joined.attempt.wait().await, Ok(()));
    }

    #[tokio::test]
    async fn dropped_owner_reports_failure_and_can_retry() {
        let coordinator = terminal_coordinator();
        let first = coordinator
            .begin(TerminalIntent::Close)
            .expect("claim close");
        drop(first.owner.expect("close owner"));
        assert_eq!(
            first.attempt.wait().await,
            Err(TerminalFailure::OwnerStopped)
        );

        let retry = coordinator
            .begin(TerminalIntent::Close)
            .expect("retry close");
        assert!(retry.owner.is_some());
    }

    #[test]
    fn claimed_terminal_rejects_new_native_skin_ingress() {
        let desktop = DesktopState::new("test".to_string());
        let terminal = desktop
            .begin_terminal(TerminalIntent::Close)
            .expect("claim close");

        assert!(desktop.native_skin_drop().try_begin_ingress().is_err());
        terminal
            .owner
            .expect("close owner")
            .finish(Err(TerminalFailure::AppShutdown));
        assert!(desktop.native_skin_drop().try_begin_ingress().is_err());
    }

    #[tokio::test]
    async fn terminal_owner_waits_for_in_flight_native_skin_ingress() {
        let desktop = DesktopState::new("test".to_string());
        let ingress = desktop
            .native_skin_drop()
            .try_begin_ingress()
            .expect("begin native skin ingress");
        let terminal = desktop
            .begin_terminal(TerminalIntent::Close)
            .expect("claim close");
        let owner = terminal.owner.expect("close owner");
        let drained = Arc::new(AtomicUsize::new(0));
        let task_drained = Arc::clone(&drained);
        let waiter = tokio::spawn(async move {
            owner.wait_for_ingress_drain().await;
            task_drained.store(1, Ordering::SeqCst);
            owner.finish(Ok(()));
        });

        tokio::task::yield_now().await;
        assert_eq!(drained.load(Ordering::SeqCst), 0);
        drop(ingress);
        waiter.await.expect("join terminal ingress drain");
        assert_eq!(drained.load(Ordering::SeqCst), 1);
        assert_eq!(terminal.attempt.wait().await, Ok(()));
    }

    #[tokio::test]
    async fn native_skin_ingress_drain_does_not_miss_release_race() {
        let desktop = DesktopState::new("test".to_string());
        for _ in 0..128 {
            let ingress = desktop
                .native_skin_drop()
                .try_begin_ingress()
                .expect("begin native skin ingress");
            let coordinator = desktop.native_skin_drop().clone();
            let waiter = tokio::spawn(async move {
                coordinator.wait_for_ingress_drain().await;
            });

            tokio::task::yield_now().await;
            drop(ingress);
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("native skin drain notification")
                .expect("join native skin drain waiter");
        }
    }

    #[tokio::test]
    async fn reset_preflight_failure_reopens_native_skin_ingress() {
        let desktop = DesktopState::new("test".to_string());
        let terminal = desktop
            .begin_terminal(TerminalIntent::Reset)
            .expect("claim reset");
        terminal
            .owner
            .expect("reset owner")
            .finish(Err(TerminalFailure::ResetPreflight));

        assert_eq!(
            terminal.attempt.wait().await,
            Err(TerminalFailure::ResetPreflight)
        );
        assert!(desktop.native_skin_drop().try_begin_ingress().is_ok());
    }
}
