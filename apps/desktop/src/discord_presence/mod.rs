mod activity;
mod client;
mod client_id;
mod transport;
mod worker;

use axial_api::state::AppState;
use axial_api::state::presence::{PresenceSnapshot, build_presence_snapshot};
use client_id::configured_client_id;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{info, warn};
use worker::{DiscordPresenceWorker, PresenceCommand};

const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct DiscordPresenceHandle {
    commands: Option<Sender<PresenceCommand>>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
    done: Arc<Mutex<Option<Receiver<()>>>>,
}

impl DiscordPresenceHandle {
    fn disabled() -> Self {
        Self {
            commands: None,
            worker: Arc::new(Mutex::new(None)),
            done: Arc::new(Mutex::new(None)),
        }
    }

    fn active(
        commands: Sender<PresenceCommand>,
        worker: JoinHandle<()>,
        done: Receiver<()>,
    ) -> Self {
        Self {
            commands: Some(commands),
            worker: Arc::new(Mutex::new(Some(worker))),
            done: Arc::new(Mutex::new(Some(done))),
        }
    }

    pub fn shutdown_blocking(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(PresenceCommand::Shutdown);
        }

        let Some(worker) = self.worker.lock().ok().and_then(|mut guard| guard.take()) else {
            return;
        };

        let completed = self
            .done
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
            .is_none_or(|done| done.recv_timeout(SHUTDOWN_WAIT).is_ok());
        if completed {
            if worker.join().is_err() {
                warn!("Discord RPC worker panicked during shutdown");
            }
        } else {
            warn!("Discord RPC worker did not stop before shutdown timeout; detaching");
        }
    }
}

pub fn spawn(state: AppState) -> DiscordPresenceHandle {
    let Some(client_id) = configured_client_id() else {
        info!(
            "Discord RPC is inactive; AXIAL_DISCORD_APPLICATION_ID was not provided at build time"
        );
        return DiscordPresenceHandle::disabled();
    };

    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    spawn_snapshot_monitor(state, tx.clone());

    match thread::Builder::new()
        .name("axial-discord-rpc".to_string())
        .spawn(move || {
            DiscordPresenceWorker::new(client_id, rx).run();
            let _ = done_tx.send(());
        }) {
        Ok(worker) => DiscordPresenceHandle::active(tx, worker, done_rx),
        Err(error) => {
            warn!(error = %error, "failed to start Discord RPC worker");
            DiscordPresenceHandle::disabled()
        }
    }
}

fn spawn_snapshot_monitor(state: AppState, tx: Sender<PresenceCommand>) {
    tokio::spawn(async move {
        let mut session_changes = state.sessions().subscribe_changes();
        let mut config_changes = state.subscribe_config_changes();
        let mut last: Option<PresenceSnapshot> = None;

        loop {
            let snapshot = build_presence_snapshot(&state).await;
            if last.as_ref() != Some(&snapshot) {
                if tx
                    .send(PresenceCommand::Snapshot(snapshot.clone()))
                    .is_err()
                {
                    break;
                }
                last = Some(snapshot);
            }

            tokio::select! {
                received = session_changes.recv() => {
                    if matches!(received, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        break;
                    }
                }
                received = config_changes.recv() => {
                    if matches!(received, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        break;
                    }
                }
            }
        }
    });
}
