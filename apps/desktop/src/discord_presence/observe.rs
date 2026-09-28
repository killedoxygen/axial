use super::{
    DiscordPresenceHandle, PresenceLoader, PresencePerformance, PresencePhase, PresenceSession,
    PresenceSnapshot, ShutdownOutcome,
};
use axial_app::{
    instances::create::InstanceService,
    launch::session::{SessionManager, SessionPhase},
    settings::{ConfigView, SettingsStore},
};
use std::sync::Arc;
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
};

/// Native IPC consumes public settings/session projections. It never owns a
/// second session state or feeds a Discord completion back into launch behavior.
#[derive(Clone)]
pub struct PresenceObserver {
    presence: DiscordPresenceHandle,
    stop: watch::Sender<bool>,
    observer: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl PresenceObserver {
    #[cfg(test)]
    pub(crate) fn disabled_for_test() -> Self {
        Self {
            presence: super::spawn_configured(None, PresenceSnapshot::from_sessions(false, &[])),
            stop: watch::channel(false).0,
            observer: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start(
        settings: Arc<SettingsStore>,
        instances: Arc<InstanceService>,
        sessions: SessionManager,
    ) -> Result<Self, String> {
        let mut settings_changes = settings
            .subscribe()
            .map_err(|_| "Could not read desktop presence preferences.")?;
        let mut session_changes = sessions.subscribe_changes();
        let initial = project(&settings_changes.borrow(), &instances, &sessions);
        let presence = super::spawn(initial);
        let (stop, mut stop_changes) = watch::channel(false);
        let observer = if presence.is_configured() {
            let publisher = presence.clone();
            Some(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        changed = stop_changes.changed() => {
                            if changed.is_err() || *stop_changes.borrow() { break; }
                        }
                        changed = settings_changes.changed() => {
                            if changed.is_err() { break; }
                        }
                        changed = session_changes.changed() => {
                            if changed.is_err() { break; }
                        }
                    }
                    publisher.publish(project(
                        &settings_changes.borrow_and_update(),
                        &instances,
                        &sessions,
                    ));
                }
            }))
        } else {
            None
        };
        Ok(Self {
            presence,
            stop,
            observer: Arc::new(Mutex::new(observer)),
        })
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.stop.send_replace(true);
        let mut observer = self.observer.lock().await;
        if let Some(join) = observer.as_mut() {
            // Keep the handle retained across a dropped shutdown waiter.
            let result = join.await;
            observer.take();
            if result.is_err() {
                tracing::warn!("Discord projection observer stopped unexpectedly");
            }
        }
        let presence = self.presence.clone();
        let outcome = tokio::task::spawn_blocking(move || presence.shutdown_blocking())
            .await
            .map_err(|_| "Desktop presence shutdown did not finish.".to_string())?;
        match outcome {
            ShutdownOutcome::Stopped | ShutdownOutcome::WorkerPanicked => Ok(()),
            ShutdownOutcome::TimedOut => {
                Err("Desktop presence is still stopping. Try the same action again.".into())
            }
        }
    }
}

fn project(
    settings: &ConfigView,
    instances: &InstanceService,
    sessions: &SessionManager,
) -> PresenceSnapshot {
    let active = sessions
        .sessions()
        .into_iter()
        .filter(|session| session.phase == SessionPhase::Starting || session.process_alive)
        .map(|session| {
            let instance = instances.registry().get_live(&session.instance_id).ok();
            let loader = instance
                .as_ref()
                .map(|record| record.instance.loader_key.as_str());
            let performance = instance
                .as_ref()
                .and_then(|record| record.instance.settings.effective(settings).ok());
            PresenceSession {
                phase: if session.process_alive {
                    PresencePhase::Playing
                } else {
                    PresencePhase::Launching
                },
                loader: match loader {
                    Some("vanilla" | "") => PresenceLoader::Vanilla,
                    Some("fabric") => PresenceLoader::Fabric,
                    Some("quilt") => PresenceLoader::Quilt,
                    Some("forge") => PresenceLoader::Forge,
                    Some("neoforge") => PresenceLoader::NeoForge,
                    _ => PresenceLoader::Modded,
                },
                minecraft_version: instance
                    .as_ref()
                    .map(|record| record.instance.minecraft_version.clone()),
                performance: match performance
                    .as_ref()
                    .map(|value| value.performance_mode.as_str())
                {
                    Some("managed") => PresencePerformance::Managed,
                    Some("vanilla") => PresencePerformance::Vanilla,
                    Some("custom") => PresencePerformance::Custom,
                    _ => PresencePerformance::Unknown,
                },
                started_at_ms: session.started_at_ms,
            }
        })
        .collect::<Vec<_>>();
    PresenceSnapshot::from_sessions(settings.discord_rpc_enabled, &active)
}
