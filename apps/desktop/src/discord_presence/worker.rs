use super::activity::discord_activity;
use super::client::DiscordRpcClient;
use super::snapshot::PresenceSnapshot;
use super::transport::DiscordRpcError;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

// The one test seam is the actual IPC boundary; projection and lifecycle run
// unchanged in tests and the native worker.
pub(super) trait RpcConnection: Sized {
    fn connect(client_id: &str) -> impl Future<Output = Result<Self, DiscordRpcError>>;
    fn set_activity(
        &mut self,
        activity: &Value,
    ) -> impl Future<Output = Result<(), DiscordRpcError>>;
    fn clear_and_close(self) -> impl Future<Output = ()>;
}

impl RpcConnection for DiscordRpcClient {
    async fn connect(client_id: &str) -> Result<Self, DiscordRpcError> {
        Self::connect(client_id).await
    }
    async fn set_activity(&mut self, activity: &Value) -> Result<(), DiscordRpcError> {
        self.set_activity(activity).await
    }
    async fn clear_and_close(self) {
        self.clear_and_close().await;
    }
}

#[derive(Clone)]
pub(super) enum Command {
    Snapshot(PresenceSnapshot),
    Shutdown,
}

#[derive(Clone, Copy)]
pub(super) struct Timing {
    initial_backoff: Duration,
    max_backoff: Duration,
    refresh_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
            refresh_interval: REFRESH_INTERVAL,
        }
    }
}

pub(super) async fn run<C: RpcConnection>(
    client_id: String,
    mut commands: watch::Receiver<Command>,
    timing: Timing,
) {
    let mut client: Option<C> = None;
    let mut last_activity: Option<Value> = None;
    let mut next_attempt = Instant::now();
    let mut backoff = timing.initial_backoff;
    loop {
        let command = commands.borrow_and_update().clone();
        let Command::Snapshot(snapshot) = command else {
            break;
        };
        if !snapshot.enabled {
            if let Some(connected) = client.take() {
                connected.clear_and_close().await;
            }
            last_activity = None;
            next_attempt = Instant::now();
            backoff = timing.initial_backoff;
            if commands.changed().await.is_err() {
                break;
            }
            continue;
        }

        if client.is_none() && Instant::now() >= next_attempt {
            match C::connect(&client_id).await {
                Ok(connected) => {
                    client = Some(connected);
                    // A disable or shutdown accepted during connection must win
                    // before a new activity can be sent.
                    if commands.has_changed().unwrap_or(true) {
                        continue;
                    }
                }
                Err(error) => {
                    tracing::debug!(error = %error, "Discord connection unavailable; retry scheduled");
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(timing.max_backoff);
                }
            }
        }
        let activity = discord_activity(&snapshot);
        if let Some(connected) = client.as_mut()
            && (last_activity.as_ref() != Some(&activity) || Instant::now() >= next_attempt)
        {
            match connected.set_activity(&activity).await {
                Ok(()) => {
                    last_activity = Some(activity);
                    backoff = timing.initial_backoff;
                    next_attempt = Instant::now() + timing.refresh_interval;
                }
                Err(error) => {
                    tracing::debug!(error = %error, "Discord presence update failed; retry scheduled");
                    // Partial frame reads cannot be resumed safely. Dropping the
                    // old connection clears its association; retry uses fresh IPC.
                    client = None;
                    last_activity = None;
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(timing.max_backoff);
                }
            }
        }
        tokio::select! {
            changed = commands.changed() => { if changed.is_err() { break; } }
            _ = tokio::time::sleep_until(next_attempt) => {}
        }
    }
    if let Some(connected) = client {
        connected.clear_and_close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct State {
        attempts: usize,
        connect_failures: usize,
        update_failures: usize,
        activities: Vec<Value>,
        clears: usize,
    }
    tokio::task_local! { static PEER: Arc<Mutex<State>>; }
    struct Peer(Arc<Mutex<State>>);
    impl RpcConnection for Peer {
        async fn connect(_: &str) -> Result<Self, DiscordRpcError> {
            PEER.with(|shared| {
                let mut state = shared.lock().unwrap();
                state.attempts += 1;
                if state.connect_failures > 0 {
                    state.connect_failures -= 1;
                    Err(DiscordRpcError::Absent)
                } else {
                    Ok(Self(shared.clone()))
                }
            })
        }
        async fn set_activity(&mut self, activity: &Value) -> Result<(), DiscordRpcError> {
            let mut state = self.0.lock().unwrap();
            if state.update_failures > 0 {
                state.update_failures -= 1;
                return Err(DiscordRpcError::Protocol);
            }
            state.activities.push(activity.clone());
            Ok(())
        }
        async fn clear_and_close(self) {
            self.0.lock().unwrap().clears += 1;
        }
    }

    fn timing() -> Timing {
        Timing {
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            refresh_interval: Duration::from_secs(60),
        }
    }

    async fn until(mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected worker state before deadline");
    }

    #[tokio::test]
    async fn absent_discord_retries_and_shutdown_clears_successful_connection() {
        let state = Arc::new(Mutex::new(State {
            connect_failures: 1,
            ..State::default()
        }));
        let (tx, rx) = watch::channel(Command::Snapshot(PresenceSnapshot::from_sessions(
            true,
            &[],
        )));
        let worker =
            tokio::spawn(PEER.scope(state.clone(), run::<Peer>("123456".into(), rx, timing())));
        until(|| state.lock().unwrap().activities.len() == 1).await;
        tx.send_replace(Command::Shutdown);
        worker.await.unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.attempts, 2);
        assert_eq!(state.clears, 1);
    }

    #[tokio::test]
    async fn disabling_clears_without_reconnecting_and_reenabling_republishes() {
        let state = Arc::new(Mutex::new(State::default()));
        let idle = PresenceSnapshot::from_sessions(true, &[]);
        let (tx, rx) = watch::channel(Command::Snapshot(idle.clone()));
        let worker =
            tokio::spawn(PEER.scope(state.clone(), run::<Peer>("123456".into(), rx, timing())));
        until(|| state.lock().unwrap().activities.len() == 1).await;
        tx.send_replace(Command::Snapshot(idle.clone()));
        tokio::task::yield_now().await;
        assert_eq!(state.lock().unwrap().activities.len(), 1);
        tx.send_replace(Command::Snapshot(PresenceSnapshot::from_sessions(
            false,
            &[],
        )));
        until(|| state.lock().unwrap().clears == 1).await;
        assert_eq!(state.lock().unwrap().attempts, 1);
        tx.send_replace(Command::Snapshot(idle));
        until(|| state.lock().unwrap().activities.len() == 2).await;
        drop(tx);
        worker.await.unwrap();
        assert_eq!(state.lock().unwrap().clears, 2);
    }

    #[tokio::test]
    async fn failed_update_reconnects_and_publishes_only_latest_snapshot() {
        let state = Arc::new(Mutex::new(State {
            update_failures: 1,
            ..State::default()
        }));
        let (tx, rx) = watch::channel(Command::Snapshot(PresenceSnapshot::from_sessions(
            true,
            &[],
        )));
        let worker =
            tokio::spawn(PEER.scope(state.clone(), run::<Peer>("123456".into(), rx, timing())));
        until(|| state.lock().unwrap().update_failures == 0).await;
        tx.send_replace(Command::Snapshot(PresenceSnapshot::from_sessions(
            false,
            &[],
        )));
        tx.send_replace(Command::Shutdown);
        worker.await.unwrap();
        assert_eq!(state.lock().unwrap().attempts, 1);
        assert!(state.lock().unwrap().activities.is_empty());
    }
}
