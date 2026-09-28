//! Native Discord IPC owner. Publishing snapshots never waits for Discord.
mod activity;
mod client;
mod client_id;
mod observe;
mod snapshot;
mod transport;
mod worker;

pub use observe::PresenceObserver;
pub use snapshot::{
    PresenceLoader, PresencePerformance, PresencePhase, PresenceSession, PresenceSnapshot,
};

use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::watch;
use worker::Command;

const SHUTDOWN_WAIT: Duration = Duration::from_secs(7);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Stopped,
    TimedOut,
    WorkerPanicked,
}

struct Worker {
    join: JoinHandle<()>,
    done: mpsc::Receiver<()>,
}

#[derive(Clone)]
pub struct DiscordPresenceHandle {
    commands: Option<watch::Sender<Command>>,
    worker: Arc<Mutex<Option<Worker>>>,
}

impl DiscordPresenceHandle {
    pub fn is_configured(&self) -> bool {
        self.commands.is_some()
    }

    /// Update the bounded latest projection. Shutdown is monotonic and cannot
    /// be undone by a racing session or settings observer.
    pub fn publish(&self, snapshot: PresenceSnapshot) {
        if let Some(commands) = &self.commands {
            commands.send_if_modified(|current| match current {
                Command::Snapshot(previous) if *previous != snapshot => {
                    *current = Command::Snapshot(snapshot.clone());
                    true
                }
                _ => false,
            });
        }
    }

    pub fn shutdown_blocking(&self) -> ShutdownOutcome {
        if let Some(commands) = &self.commands {
            commands.send_replace(Command::Shutdown);
        }
        let mut guard = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(worker) = guard.as_ref() else {
            return ShutdownOutcome::Stopped;
        };
        if matches!(
            worker.done.recv_timeout(SHUTDOWN_WAIT),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            // Retain the join handle. Shell exit/update must refuse on this
            // result and may retry shutdown after the worker settles.
            return ShutdownOutcome::TimedOut;
        }
        match guard.take().expect("retained worker").join.join() {
            Ok(()) => ShutdownOutcome::Stopped,
            Err(_) => ShutdownOutcome::WorkerPanicked,
        }
    }
}

pub fn spawn(initial: PresenceSnapshot) -> DiscordPresenceHandle {
    spawn_configured(client_id::configured_client_id(), initial)
}

fn spawn_configured(client_id: Option<String>, initial: PresenceSnapshot) -> DiscordPresenceHandle {
    let disabled = || DiscordPresenceHandle {
        commands: None,
        worker: Arc::new(Mutex::new(None)),
    };
    let Some(client_id) = client_id else {
        return disabled();
    };
    let (commands, receiver) = watch::channel(Command::Snapshot(initial));
    let (done_tx, done) = mpsc::channel();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            tracing::warn!("Discord presence runtime could not start");
            return disabled();
        }
    };
    match thread::Builder::new()
        .name("axial-discord-rpc".into())
        .spawn(move || {
            runtime.block_on(worker::run::<client::DiscordRpcClient>(
                client_id,
                receiver,
                worker::Timing::default(),
            ));
            let _ = done_tx.send(());
        }) {
        Ok(join) => DiscordPresenceHandle {
            commands: Some(commands),
            worker: Arc::new(Mutex::new(Some(Worker { join, done }))),
        },
        Err(_) => {
            tracing::warn!("Discord presence worker could not start");
            disabled()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_configuration_starts_no_worker_and_shutdown_is_idempotent() {
        let handle = spawn_configured(None, PresenceSnapshot::from_sessions(true, &[]));
        assert!(!handle.is_configured());
        handle.publish(PresenceSnapshot::from_sessions(false, &[]));
        assert_eq!(handle.shutdown_blocking(), ShutdownOutcome::Stopped);
        assert_eq!(handle.shutdown_blocking(), ShutdownOutcome::Stopped);
    }

    #[test]
    fn configured_disabled_worker_joins_and_cannot_restart_after_shutdown() {
        let handle = spawn_configured(
            Some("123456789012345678".into()),
            PresenceSnapshot::from_sessions(false, &[]),
        );
        assert!(handle.is_configured());
        assert_eq!(handle.shutdown_blocking(), ShutdownOutcome::Stopped);
        handle.publish(PresenceSnapshot::from_sessions(true, &[]));
        assert!(matches!(
            *handle.commands.as_ref().unwrap().borrow(),
            Command::Shutdown
        ));
        assert_eq!(handle.shutdown_blocking(), ShutdownOutcome::Stopped);
    }
}
