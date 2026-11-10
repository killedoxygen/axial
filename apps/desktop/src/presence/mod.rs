//! Native Discord Rich Presence. It consumes public settings and session
//! projections, never feeds Discord results back into launch behaviour, and
//! publishing a projection never waits for Discord.
mod activity;
mod ipc;

use activity::{Loader, Performance, Phase, Session, Snapshot};
use axial_app::{
    instances::create::InstanceService,
    launch::session::{SessionManager, SessionPhase},
    settings::{ConfigView, SettingsStore},
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
    time::Instant,
};

const SHUTDOWN_WAIT: Duration = Duration::from_secs(7);
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Shutdown is monotonic. A worker that has not stopped in time stays
/// retained, so the next attempt joins the same task.
#[derive(Clone)]
pub struct Presence {
    stop: Arc<watch::Sender<bool>>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Presence {
    pub fn start(
        settings: &SettingsStore,
        instances: Arc<InstanceService>,
        sessions: SessionManager,
    ) -> Result<Self, String> {
        let mut settings_changes = settings
            .subscribe()
            .map_err(|_| "Could not read desktop presence preferences.")?;
        let (stop, mut stopping) = watch::channel(false);
        let worker = configured_client_id().map(|client_id| {
            let mut session_changes = sessions.subscribe_changes();
            let initial = project(&settings_changes.borrow_and_update(), &instances, &sessions);
            let (publish, snapshots) = watch::channel(initial);
            let observe = async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = stopping.wait_for(|stop| *stop) => break,
                        changed = settings_changes.changed() => if changed.is_err() { break },
                        changed = session_changes.changed() => if changed.is_err() { break },
                    }
                    let next =
                        project(&settings_changes.borrow_and_update(), &instances, &sessions);
                    publish.send_if_modified(|current| {
                        let changed = *current != next;
                        *current = next;
                        changed
                    });
                }
                // Dropping the publisher stops the worker once it clears Discord.
            };
            let run = run(client_id, ipc::candidates(), INITIAL_BACKOFF, snapshots);
            tokio::spawn(async move {
                tokio::join!(observe, run);
            })
        });
        Ok(Self {
            stop: Arc::new(stop),
            worker: Arc::new(Mutex::new(worker)),
        })
    }

    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            stop: Arc::new(watch::channel(false).0),
            worker: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.stop.send_replace(true);
        let mut worker = self.worker.lock().await;
        let Some(join) = worker.as_mut() else {
            return Ok(());
        };
        let joined = tokio::time::timeout(SHUTDOWN_WAIT, join)
            .await
            .map_err(|_| "Desktop presence is still stopping. Try the same action again.")?;
        worker.take();
        if joined.is_err() {
            tracing::warn!("Discord presence stopped unexpectedly");
        }
        Ok(())
    }
}

fn configured_client_id() -> Option<String> {
    let value = sanitize_client_id(option_env!("AXIAL_DISCORD_APPLICATION_ID")?);
    if value.is_none() {
        tracing::warn!("Discord application ID is invalid; presence is inactive");
    }
    value
}

fn sanitize_client_id(raw: &str) -> Option<String> {
    let value = raw.trim();
    ((6..=32).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.to_string())
}

fn project(
    settings: &ConfigView,
    instances: &InstanceService,
    sessions: &SessionManager,
) -> Snapshot {
    let active = sessions
        .sessions()
        .into_iter()
        .filter(|session| session.phase == SessionPhase::Starting || session.process_alive)
        .map(|session| {
            let instance = instances.registry().get_live(&session.instance_id).ok();
            let instance = instance.as_ref().map(|record| &record.instance);
            Session {
                phase: if session.process_alive {
                    Phase::Playing
                } else {
                    Phase::Launching
                },
                loader: match instance.map(|instance| instance.loader_key.as_str()) {
                    Some("vanilla" | "") => Loader::Vanilla,
                    Some("fabric") => Loader::Fabric,
                    Some("quilt") => Loader::Quilt,
                    Some("forge") => Loader::Forge,
                    Some("neoforge") => Loader::NeoForge,
                    _ => Loader::Modded,
                },
                minecraft_version: instance.map(|instance| instance.minecraft_version.clone()),
                performance: match instance
                    .and_then(|instance| instance.settings.effective(settings).ok())
                    .as_ref()
                    .map(|effective| effective.performance_mode.as_str())
                {
                    Some("managed") => Performance::Managed,
                    Some("vanilla") => Performance::Vanilla,
                    Some("custom") => Performance::Custom,
                    _ => Performance::Unknown,
                },
                started_at_ms: session.started_at_ms,
            }
        })
        .collect::<Vec<_>>();
    Snapshot::from_sessions(settings.discord_rpc_enabled, &active)
}

/// Publishes the latest snapshot until its publisher closes, then clears the
/// activity. A closed publisher wins over any snapshot it left behind.
async fn run(
    client_id: String,
    paths: Vec<PathBuf>,
    initial_backoff: Duration,
    mut snapshots: watch::Receiver<Snapshot>,
) {
    let mut client: Option<ipc::Client> = None;
    let mut sent: Option<Value> = None;
    let mut next_attempt = Instant::now();
    let mut backoff = initial_backoff;
    while snapshots.has_changed().is_ok() {
        let snapshot = snapshots.borrow_and_update().clone();
        if !snapshot.enabled {
            if let Some(connected) = client.take() {
                connected.clear_and_close().await;
            }
            sent = None;
            next_attempt = Instant::now();
            backoff = initial_backoff;
            if snapshots.changed().await.is_err() {
                break;
            }
            continue;
        }
        if client.is_none() && Instant::now() >= next_attempt {
            match ipc::Client::connect(&client_id, &paths).await {
                Ok(connected) => {
                    client = Some(connected);
                    // A disable or shutdown accepted while connecting must win
                    // before a new activity can be sent.
                    if snapshots.has_changed().unwrap_or(true) {
                        continue;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "Discord connection unavailable; retry scheduled");
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        let activity = snapshot.activity();
        if let Some(connected) = client.as_mut()
            && (sent.as_ref() != Some(&activity) || Instant::now() >= next_attempt)
        {
            match connected.set_activity(&activity).await {
                Ok(()) => {
                    sent = Some(activity);
                    backoff = initial_backoff;
                    next_attempt = Instant::now() + REFRESH_INTERVAL;
                }
                Err(error) => {
                    tracing::debug!(%error, "Discord presence update failed; retry scheduled");
                    // A partial frame read cannot be resumed. Dropping the old
                    // connection clears its association; retry uses fresh IPC.
                    client = None;
                    sent = None;
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        tokio::select! {
            changed = snapshots.changed() => if changed.is_err() { break },
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

    #[test]
    fn only_bounded_numeric_application_ids_enable_integration() {
        assert_eq!(
            sanitize_client_id(" 123456789012345678 ").as_deref(),
            Some("123456789012345678")
        );
        for invalid in ["", "12345", "abc123", "12345678901234567890123456789012345"] {
            assert_eq!(sanitize_client_id(invalid), None);
        }
    }

    #[cfg(unix)]
    mod discord {
        use super::super::*;
        use ipc::{OP_FRAME, OP_HANDSHAKE, read_frame, write_frame};
        use serde_json::json;
        use tokio::sync::mpsc;

        #[derive(Debug, PartialEq)]
        enum Seen {
            Connected,
            Activity(Value),
            Closed,
        }

        /// A real local Discord peer that records each connection's commands.
        fn listen(path: &std::path::Path) -> mpsc::UnboundedReceiver<Seen> {
            let listener = tokio::net::UnixListener::bind(path).unwrap();
            let (seen, events) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let (opcode, handshake) = read_frame(&mut stream).await.unwrap();
                    assert_eq!(opcode, OP_HANDSHAKE);
                    assert_eq!(handshake, json!({ "v": 1, "client_id": "123456" }));
                    write_frame(&mut stream, OP_FRAME, &json!({ "evt": "READY" }))
                        .await
                        .unwrap();
                    let _ = seen.send(Seen::Connected);
                    while let Ok((OP_FRAME, command)) = read_frame(&mut stream).await {
                        let _ = seen.send(Seen::Activity(command["args"]["activity"].clone()));
                        let reply = json!({ "nonce": command["nonce"] });
                        write_frame(&mut stream, OP_FRAME, &reply).await.unwrap();
                    }
                    let _ = seen.send(Seen::Closed);
                }
            });
            events
        }

        async fn expect(events: &mut mpsc::UnboundedReceiver<Seen>, expected: &[Seen]) {
            for expected in expected {
                let seen = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
                assert_eq!(seen.unwrap().as_ref(), Some(expected));
            }
        }

        fn socket() -> (tempfile::TempDir, PathBuf) {
            let directory =
                tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let path = directory.path().join("discord-ipc-0");
            (directory, path)
        }

        #[tokio::test]
        async fn absent_discord_is_retried_and_shutdown_clears_the_activity() {
            let (_directory, path) = socket();
            let idle = Snapshot::from_sessions(true, &[]);
            let (publish, snapshots) = watch::channel(idle.clone());
            let worker = tokio::spawn(run(
                "123456".into(),
                vec![path.clone()],
                Duration::from_millis(5),
                snapshots,
            ));
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut events = listen(&path);
            expect(
                &mut events,
                &[Seen::Connected, Seen::Activity(idle.activity())],
            )
            .await;
            drop(publish);
            worker.await.unwrap();
            expect(&mut events, &[Seen::Activity(Value::Null), Seen::Closed]).await;
        }

        #[tokio::test]
        async fn disabling_clears_and_reenabling_reconnects() {
            let (_directory, path) = socket();
            let mut events = listen(&path);
            let idle = Snapshot::from_sessions(true, &[]);
            let (publish, snapshots) = watch::channel(idle.clone());
            let worker = tokio::spawn(run(
                "123456".into(),
                vec![path],
                Duration::from_millis(5),
                snapshots,
            ));
            let published = [Seen::Connected, Seen::Activity(idle.activity())];
            let cleared = [Seen::Activity(Value::Null), Seen::Closed];
            expect(&mut events, &published).await;
            publish.send_replace(Snapshot::from_sessions(false, &[]));
            expect(&mut events, &cleared).await;
            publish.send_replace(idle.clone());
            expect(&mut events, &published).await;
            drop(publish);
            worker.await.unwrap();
            expect(&mut events, &cleared).await;
        }
    }
}
