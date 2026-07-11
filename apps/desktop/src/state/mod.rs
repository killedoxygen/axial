use axial_api::app::{ApiServerShutdownError, ServerHandle};
use axial_config::AppPaths;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const TERMINAL_STATE_LOCK_INVARIANT: &str =
    "desktop terminal-state lock poisoned; action ownership may be inconsistent";

#[derive(Clone)]
pub struct DesktopState {
    version: String,
    paths: AppPaths,
    terminal: TerminalActionCoordinator,
}

impl DesktopState {
    pub fn new(version: String, paths: AppPaths) -> Self {
        Self {
            version,
            paths,
            terminal: TerminalActionCoordinator::new(),
        }
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn terminal(&self) -> &TerminalActionCoordinator {
        &self.terminal
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
    fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(TerminalActionState {
                intent: None,
                active: None,
                completed: None,
            })),
        }
    }

    pub fn begin(
        &self,
        intent: TerminalIntent,
    ) -> Result<TerminalAttemptStart, TerminalIntentConflict> {
        let mut state = self.shared.lock().expect(TERMINAL_STATE_LOCK_INVARIANT);
        match state.intent {
            Some(active) if active != intent => {
                return Err(TerminalIntentConflict {
                    active,
                    requested: intent,
                });
            }
            None => state.intent = Some(intent),
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

    pub fn is_claimed(&self, intent: TerminalIntent) -> bool {
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
        state.completed = Some(result);
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
    use super::{TerminalActionCoordinator, TerminalFailure, TerminalIntent};

    #[tokio::test]
    async fn same_intent_joins_one_active_attempt() {
        let coordinator = TerminalActionCoordinator::new();
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
        let coordinator = TerminalActionCoordinator::new();
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
        let coordinator = TerminalActionCoordinator::new();
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
    async fn dropping_a_waiter_does_not_abandon_the_owner() {
        let coordinator = TerminalActionCoordinator::new();
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
        let coordinator = TerminalActionCoordinator::new();
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
}
