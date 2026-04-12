use croopor_minecraft::download::DownloadProgress;
use std::collections::HashMap;
use tokio::sync::{RwLock, broadcast};

struct InstallEntry {
    history: Vec<DownloadProgress>,
    events: broadcast::Sender<DownloadProgress>,
    done: bool,
}

pub struct InstallStore {
    installs: RwLock<HashMap<String, InstallEntry>>,
}

impl InstallStore {
    pub fn new() -> Self {
        Self {
            installs: RwLock::new(HashMap::new()),
        }
    }

    pub async fn insert(&self, install_id: String) {
        let (events, _) = broadcast::channel(256);
        let mut installs = self.installs.write().await;
        installs.insert(
            install_id,
            InstallEntry {
                history: Vec::new(),
                events,
                done: false,
            },
        );
    }

    pub async fn emit(&self, install_id: &str, progress: DownloadProgress) {
        let mut installs = self.installs.write().await;
        if let Some(entry) = installs.get_mut(install_id) {
            entry.done = progress.done;
            entry.history.push(progress.clone());
            let _ = entry.events.send(progress);
        }
    }

    pub async fn subscribe(
        &self,
        install_id: &str,
    ) -> Option<(
        Vec<DownloadProgress>,
        broadcast::Receiver<DownloadProgress>,
        bool,
    )> {
        self.installs
            .read()
            .await
            .get(install_id)
            .map(|entry| (entry.history.clone(), entry.events.subscribe(), entry.done))
    }

    pub async fn remove(&self, install_id: &str) {
        self.installs.write().await.remove(install_id);
    }

    pub async fn clear(&self) {
        self.installs.write().await.clear();
    }
}

impl Default for InstallStore {
    fn default() -> Self {
        Self::new()
    }
}
