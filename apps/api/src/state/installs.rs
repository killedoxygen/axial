use croopor_minecraft::download::DownloadProgress;
use std::collections::HashMap;
use tokio::sync::{RwLock, broadcast};

struct InstallEntry {
    key: Option<InstallKey>,
    history: Vec<DownloadProgress>,
    events: broadcast::Sender<DownloadProgress>,
    done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallKey {
    scope: String,
    version_id: String,
    manifest_url: String,
}

impl InstallKey {
    fn new(scope: String, version_id: String, manifest_url: String) -> Self {
        Self {
            scope: scope.trim().to_string(),
            version_id: version_id.trim().to_string(),
            manifest_url: manifest_url.trim().to_string(),
        }
    }
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
        self.insert_entry(install_id, None).await;
    }

    pub async fn insert_or_existing_active(
        &self,
        install_id: String,
        version_id: String,
        manifest_url: String,
    ) -> (String, bool) {
        self.insert_or_existing_active_scoped(
            "vanilla".to_string(),
            install_id,
            version_id,
            manifest_url,
        )
        .await
    }

    pub async fn insert_or_existing_active_scoped(
        &self,
        scope: String,
        install_id: String,
        version_id: String,
        manifest_url: String,
    ) -> (String, bool) {
        let key = InstallKey::new(scope, version_id, manifest_url);
        let mut installs = self.installs.write().await;
        prune_done_entries(&mut installs);
        if let Some(existing_id) = installs.iter().find_map(|(existing_id, entry)| {
            (!entry.done && entry.key.as_ref() == Some(&key)).then(|| existing_id.clone())
        }) {
            return (existing_id, false);
        }

        installs.insert(install_id.clone(), new_install_entry(Some(key)));
        (install_id, true)
    }

    async fn insert_entry(&self, install_id: String, key: Option<InstallKey>) {
        let mut installs = self.installs.write().await;
        prune_done_entries(&mut installs);
        installs.insert(install_id, new_install_entry(key));
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

    pub async fn active_install_count(&self) -> usize {
        self.installs
            .read()
            .await
            .values()
            .filter(|entry| !entry.done)
            .count()
    }

    pub async fn remove(&self, install_id: &str) {
        self.installs.write().await.remove(install_id);
    }

    pub async fn clear(&self) {
        self.installs.write().await.clear();
    }
}

fn new_install_entry(key: Option<InstallKey>) -> InstallEntry {
    let (events, _) = broadcast::channel(256);
    InstallEntry {
        key,
        history: Vec::new(),
        events,
        done: false,
    }
}

fn prune_done_entries(installs: &mut HashMap<String, InstallEntry>) {
    installs.retain(|_, entry| !entry.done);
}

impl Default for InstallStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn install_insert_or_existing_reuses_active_matching_key() {
        let store = InstallStore::new();
        let (first_id, first_inserted) = store
            .insert_or_existing_active(
                "first-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        let (second_id, second_inserted) = store
            .insert_or_existing_active(
                "second-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(first_id, "first-install");
        assert!(first_inserted);
        assert_eq!(second_id, "first-install");
        assert!(!second_inserted);
        assert_eq!(store.active_install_count().await, 1);
        assert!(store.subscribe("second-install").await.is_none());
    }

    #[tokio::test]
    async fn install_insert_prunes_done_entries() {
        let store = InstallStore::new();
        store.insert("done-install".to_string()).await;
        store.insert("active-install".to_string()).await;
        store.emit("done-install", done_progress()).await;

        store.insert("fresh-install".to_string()).await;

        assert!(store.subscribe("done-install").await.is_none());
        assert!(store.subscribe("active-install").await.is_some());
        assert!(store.subscribe("fresh-install").await.is_some());
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_insert_or_existing_prunes_done_entries_and_reuses_active_match() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "done-install".to_string(),
                "1.21.4".to_string(),
                String::new(),
            )
            .await;
        store
            .insert_or_existing_active(
                "active-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("done-install", done_progress()).await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "duplicate-active-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(install_id, "active-install");
        assert!(!inserted);
        assert!(store.subscribe("done-install").await.is_none());
        assert!(store.subscribe("duplicate-active-install").await.is_none());
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn install_insert_or_existing_trims_matching_key_fields() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "trimmed-install".to_string(),
                " 1.21.5 ".to_string(),
                " https://example.invalid/manifest.json ".to_string(),
            )
            .await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "duplicate-install".to_string(),
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string(),
            )
            .await;

        assert_eq!(install_id, "trimmed-install");
        assert!(!inserted);
    }

    #[tokio::test]
    async fn install_insert_or_existing_allows_fresh_install_after_done() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "done-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("done-install", done_progress()).await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "fresh-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(install_id, "fresh-install");
        assert!(inserted);
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn install_insert_or_existing_allows_fresh_install_after_remove() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "removed-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.remove("removed-install").await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "fresh-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(install_id, "fresh-install");
        assert!(inserted);
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn install_insert_or_existing_keeps_different_versions_independent() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "first-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "second-install".to_string(),
                "1.21.6".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(install_id, "second-install");
        assert!(inserted);
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_insert_or_existing_keeps_manifest_urls_independent() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "normal-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        let (explicit_id, explicit_inserted) = store
            .insert_or_existing_active(
                "explicit-install".to_string(),
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string(),
            )
            .await;
        let (duplicate_explicit_id, duplicate_explicit_inserted) = store
            .insert_or_existing_active(
                "duplicate-explicit-install".to_string(),
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string(),
            )
            .await;

        assert_eq!(explicit_id, "explicit-install");
        assert!(explicit_inserted);
        assert_eq!(duplicate_explicit_id, "explicit-install");
        assert!(!duplicate_explicit_inserted);
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_insert_or_existing_keeps_scopes_independent() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "vanilla-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        let (loader_id, loader_inserted) = store
            .insert_or_existing_active_scoped(
                "loader".to_string(),
                "loader-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        let (duplicate_loader_id, duplicate_loader_inserted) = store
            .insert_or_existing_active_scoped(
                " loader ".to_string(),
                "duplicate-loader-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(loader_id, "loader-install");
        assert!(loader_inserted);
        assert_eq!(duplicate_loader_id, "loader-install");
        assert!(!duplicate_loader_inserted);
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn launch_active_install_count_excludes_done_sessions() {
        let store = InstallStore::new();
        store.insert("active-install".to_string()).await;
        store.insert("done-install".to_string()).await;
        store
            .emit(
                "done-install",
                DownloadProgress {
                    phase: "done".to_string(),
                    current: 1,
                    total: 1,
                    file: None,
                    error: None,
                    done: true,
                },
            )
            .await;

        assert_eq!(store.active_install_count().await, 1);
    }

    fn done_progress() -> DownloadProgress {
        DownloadProgress {
            phase: "done".to_string(),
            current: 1,
            total: 1,
            file: None,
            error: None,
            done: true,
        }
    }
}
