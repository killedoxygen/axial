use crate::state::AppState;
use croopor_config::{AppConfig, Instance};

pub(super) fn persist_launch_metadata(
    state: &AppState,
    instance: &mut Instance,
    config: &AppConfig,
    username: &str,
    max_memory_mb: i32,
    min_memory_mb: i32,
    launched_at: &str,
) {
    instance.last_played_at = launched_at.to_string();
    let _ = state.instances().update(instance.clone());
    let _ = state.instances().set_last_instance_id(instance.id.clone());

    let mut next = config.clone();
    next.username = username.to_string();
    if max_memory_mb > 0 {
        next.max_memory_mb = max_memory_mb;
    }
    if min_memory_mb > 0 {
        next.min_memory_mb = min_memory_mb;
    }
    let _ = state.update_config(next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn persist_launch_metadata_updates_instance_last_played_last_instance_and_config() {
        let root = unique_test_dir("launch-metadata-persistence");
        let state = test_app_state(&root);
        let mut instance = state
            .instances()
            .add(
                "Launch Metadata".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let mut config = state.config().current();
        config.username = "BeforeLaunch".to_string();
        config.max_memory_mb = 3072;
        config.min_memory_mb = 512;
        config.theme = "existing-theme".to_string();
        state.update_config(config.clone()).expect("seed config");

        persist_launch_metadata(
            &state,
            &mut instance,
            &config,
            "AfterLaunch",
            6144,
            1024,
            "2026-01-01T00:00:00.000Z",
        );

        let stored = state
            .instances()
            .get(&instance.id)
            .expect("stored instance");
        assert_eq!(instance.last_played_at, "2026-01-01T00:00:00.000Z");
        assert_eq!(stored.last_played_at, "2026-01-01T00:00:00.000Z");
        assert_eq!(
            state.instances().last_instance_id().as_deref(),
            Some(instance.id.as_str())
        );

        let updated = state.config().current();
        assert_eq!(updated.username, "AfterLaunch");
        assert_eq!(updated.max_memory_mb, 6144);
        assert_eq!(updated.min_memory_mb, 1024);
        assert_eq!(updated.theme, "existing-theme");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persist_launch_metadata_keeps_existing_memory_when_new_values_are_not_positive() {
        let root = unique_test_dir("launch-metadata-non-positive-memory");
        let state = test_app_state(&root);
        let mut instance = state
            .instances()
            .add(
                "Launch Metadata".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let mut config = state.config().current();
        config.max_memory_mb = 4096;
        config.min_memory_mb = 768;
        state.update_config(config.clone()).expect("seed config");

        persist_launch_metadata(
            &state,
            &mut instance,
            &config,
            "MemoryKept",
            0,
            -1,
            "2026-01-01T00:00:00.000Z",
        );

        let updated = state.config().current();
        assert_eq!(updated.username, "MemoryKept");
        assert_eq!(updated.max_memory_mb, 4096);
        assert_eq!(updated.min_memory_mb, 768);

        let _ = fs::remove_dir_all(root);
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Croopor".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
