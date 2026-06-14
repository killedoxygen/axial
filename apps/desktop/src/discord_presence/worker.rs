use super::activity::discord_activity;
use super::client::DiscordRpcClient;
use super::transport::DiscordRpcError;
use croopor_api::state::presence::PresenceSnapshot;
use serde_json::Value;
use std::marker::PhantomData;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tracing::debug;

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(60);
const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const WORKER_IDLE_WAIT: Duration = Duration::from_secs(5 * 60);

pub(super) trait RpcConnection: Sized {
    fn connect(client_id: &str) -> Result<Self, DiscordRpcError>;
    fn set_activity(&mut self, activity: &Value) -> Result<(), DiscordRpcError>;
    fn clear_activity(&mut self) -> Result<(), DiscordRpcError>;
    fn close(&mut self) -> Result<(), DiscordRpcError>;
}

impl RpcConnection for DiscordRpcClient {
    fn connect(client_id: &str) -> Result<Self, DiscordRpcError> {
        DiscordRpcClient::connect(client_id)
    }

    fn set_activity(&mut self, activity: &Value) -> Result<(), DiscordRpcError> {
        self.set_activity(activity)
    }

    fn clear_activity(&mut self) -> Result<(), DiscordRpcError> {
        self.clear_activity()
    }

    fn close(&mut self) -> Result<(), DiscordRpcError> {
        self.close()
    }
}

#[derive(Clone, Copy)]
struct WorkerTiming {
    initial_reconnect_backoff: Duration,
    max_reconnect_backoff: Duration,
    presence_refresh_interval: Duration,
    idle_wait: Duration,
}

impl Default for WorkerTiming {
    fn default() -> Self {
        Self {
            initial_reconnect_backoff: INITIAL_RECONNECT_BACKOFF,
            max_reconnect_backoff: MAX_RECONNECT_BACKOFF,
            presence_refresh_interval: PRESENCE_REFRESH_INTERVAL,
            idle_wait: WORKER_IDLE_WAIT,
        }
    }
}

pub(super) struct DiscordPresenceWorker<C = DiscordRpcClient> {
    client_id: String,
    commands: Receiver<PresenceCommand>,
    timing: WorkerTiming,
    _client: PhantomData<C>,
}

pub(super) enum PresenceCommand {
    Snapshot(PresenceSnapshot),
    Shutdown,
}

impl DiscordPresenceWorker<DiscordRpcClient> {
    pub(super) fn new(client_id: String, commands: Receiver<PresenceCommand>) -> Self {
        Self {
            client_id,
            commands,
            timing: WorkerTiming::default(),
            _client: PhantomData,
        }
    }
}

impl<C: RpcConnection> DiscordPresenceWorker<C> {
    pub(super) fn run(self) {
        let mut current = match self.commands.recv() {
            Ok(PresenceCommand::Snapshot(snapshot)) => snapshot,
            Ok(PresenceCommand::Shutdown) => return,
            Err(_) => return,
        };
        let mut client: Option<C> = None;
        let mut last_activity: Option<Value> = None;
        let mut last_success: Option<Instant> = None;
        let mut reconnect_at = Instant::now();
        let mut reconnect_backoff = self.timing.initial_reconnect_backoff;

        loop {
            if current.enabled {
                let connected = ensure_connected(
                    &self.client_id,
                    &mut client,
                    &mut reconnect_at,
                    &mut reconnect_backoff,
                    self.timing,
                );
                if connected {
                    apply_snapshot(
                        &current,
                        &mut client,
                        &mut last_activity,
                        &mut last_success,
                        &mut reconnect_at,
                        &mut reconnect_backoff,
                        self.timing,
                    );
                }
            } else {
                clear_connected_activity(client.take());
                last_activity = None;
                last_success = None;
                reconnect_backoff = self.timing.initial_reconnect_backoff;
                reconnect_at = Instant::now();
            }

            let wait = next_wait(current.enabled, client.is_some(), reconnect_at, self.timing);
            match self.commands.recv_timeout(wait) {
                Ok(PresenceCommand::Snapshot(snapshot)) => current = snapshot,
                Ok(PresenceCommand::Shutdown) => {
                    clear_connected_activity(client);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    clear_connected_activity(client);
                    return;
                }
            }
        }
    }
}

fn ensure_connected<C: RpcConnection>(
    client_id: &str,
    client: &mut Option<C>,
    reconnect_at: &mut Instant,
    reconnect_backoff: &mut Duration,
    timing: WorkerTiming,
) -> bool {
    if client.is_some() {
        return true;
    }
    if Instant::now() < *reconnect_at {
        return false;
    }

    match C::connect(client_id) {
        Ok(next) => {
            *client = Some(next);
            *reconnect_backoff = timing.initial_reconnect_backoff;
            true
        }
        Err(error) => {
            schedule_retry(reconnect_at, reconnect_backoff, "connect", &error, timing);
            false
        }
    }
}

fn apply_snapshot<C: RpcConnection>(
    current: &PresenceSnapshot,
    client: &mut Option<C>,
    last_activity: &mut Option<Value>,
    last_success: &mut Option<Instant>,
    reconnect_at: &mut Instant,
    reconnect_backoff: &mut Duration,
    timing: WorkerTiming,
) {
    let Some(connected) = client.as_mut() else {
        return;
    };

    let activity = discord_activity(current);
    let stale = last_success
        .map(|sent_at| sent_at.elapsed() >= timing.presence_refresh_interval)
        .unwrap_or(true);
    if last_activity.as_ref() == Some(&activity) && !stale {
        return;
    }

    match connected.set_activity(&activity) {
        Ok(()) => {
            *last_activity = Some(activity);
            *last_success = Some(Instant::now());
            *reconnect_backoff = timing.initial_reconnect_backoff;
        }
        Err(error) => {
            debug!(error = %error, "Discord RPC update failed; will retry");
            if let Some(mut connected) = client.take() {
                let _ = connected.close();
            }
            *last_activity = None;
            *last_success = None;
            schedule_retry(reconnect_at, reconnect_backoff, "update", &error, timing);
        }
    }
}

fn clear_connected_activity<C: RpcConnection>(client: Option<C>) {
    if let Some(mut connected) = client {
        let _ = connected.clear_activity();
        let _ = connected.close();
    }
}

fn next_wait(
    enabled: bool,
    connected: bool,
    reconnect_at: Instant,
    timing: WorkerTiming,
) -> Duration {
    if enabled && !connected {
        reconnect_at.saturating_duration_since(Instant::now())
    } else {
        timing.idle_wait
    }
}

fn schedule_retry(
    reconnect_at: &mut Instant,
    reconnect_backoff: &mut Duration,
    action: &str,
    error: &DiscordRpcError,
    timing: WorkerTiming,
) {
    debug!(action, error = %error, "Discord RPC attempt failed");
    *reconnect_at = Instant::now() + *reconnect_backoff;
    *reconnect_backoff = (*reconnect_backoff * 2).min(timing.max_reconnect_backoff);
}

#[cfg(test)]
mod tests {
    use super::*;
    use croopor_api::state::presence::{PresenceActivity, PresenceActivityKind};
    use std::sync::Mutex;
    use std::thread;

    struct FakeRpc;

    #[derive(Clone, Copy)]
    struct FakeRpcState {
        connect_attempts: usize,
        failures_remaining: usize,
        set_activity_count: usize,
        clear_activity_count: usize,
        close_count: usize,
    }

    static FAKE_RPC_STATE: Mutex<FakeRpcState> = Mutex::new(FakeRpcState {
        connect_attempts: 0,
        failures_remaining: 0,
        set_activity_count: 0,
        clear_activity_count: 0,
        close_count: 0,
    });
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    impl RpcConnection for FakeRpc {
        fn connect(_client_id: &str) -> Result<Self, DiscordRpcError> {
            let mut state = FAKE_RPC_STATE.lock().expect("fake state lock");
            state.connect_attempts += 1;
            if state.failures_remaining > 0 {
                state.failures_remaining -= 1;
                return Err(DiscordRpcError::Connect("not ready".to_string()));
            }
            Ok(Self)
        }

        fn set_activity(&mut self, _activity: &Value) -> Result<(), DiscordRpcError> {
            FAKE_RPC_STATE
                .lock()
                .expect("fake state lock")
                .set_activity_count += 1;
            Ok(())
        }

        fn clear_activity(&mut self) -> Result<(), DiscordRpcError> {
            FAKE_RPC_STATE
                .lock()
                .expect("fake state lock")
                .clear_activity_count += 1;
            Ok(())
        }

        fn close(&mut self) -> Result<(), DiscordRpcError> {
            FAKE_RPC_STATE.lock().expect("fake state lock").close_count += 1;
            Ok(())
        }
    }

    fn reset_fake_rpc(failures_remaining: usize) {
        *FAKE_RPC_STATE.lock().expect("fake state lock") = FakeRpcState {
            connect_attempts: 0,
            failures_remaining,
            set_activity_count: 0,
            clear_activity_count: 0,
            close_count: 0,
        };
    }

    fn fake_state() -> FakeRpcState {
        *FAKE_RPC_STATE.lock().expect("fake state lock")
    }

    fn playing_snapshot() -> PresenceSnapshot {
        PresenceSnapshot {
            enabled: true,
            activity: PresenceActivity {
                kind: PresenceActivityKind::Playing,
                details: "Minecraft is running".to_string(),
                state: "Fabric 1.21.1 - Managed".to_string(),
                active_count: 1,
                started_at_unix_seconds: Some(1_781_350_000),
            },
        }
    }

    fn test_timing() -> WorkerTiming {
        WorkerTiming {
            initial_reconnect_backoff: Duration::from_millis(5),
            max_reconnect_backoff: Duration::from_millis(5),
            presence_refresh_interval: Duration::from_secs(60),
            idle_wait: Duration::from_millis(20),
        }
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("condition was not reached before deadline");
    }

    #[test]
    fn worker_reconnects_after_failed_connect_and_clears_on_shutdown() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        reset_fake_rpc(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = DiscordPresenceWorker::<FakeRpc> {
            client_id: "123456789012345678".to_string(),
            commands: rx,
            timing: test_timing(),
            _client: PhantomData,
        };
        let join = thread::spawn(move || worker.run());

        tx.send(PresenceCommand::Snapshot(playing_snapshot()))
            .expect("snapshot should send");
        wait_until(|| fake_state().set_activity_count == 1);

        tx.send(PresenceCommand::Shutdown)
            .expect("shutdown should send");
        join.join().expect("worker should join");

        let state = fake_state();
        assert_eq!(state.connect_attempts, 2);
        assert_eq!(state.set_activity_count, 1);
        assert_eq!(state.clear_activity_count, 1);
        assert_eq!(state.close_count, 1);
    }
}
