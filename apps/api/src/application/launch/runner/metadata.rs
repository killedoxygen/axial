use crate::state::{AppState, IntegrityForegroundLease};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LaunchMetadataPersistenceError {
    InstanceHistory,
    Config,
}

pub(super) async fn persist_launch_metadata(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    instance_id: &str,
    username: &str,
    max_memory_mb: i32,
    min_memory_mb: i32,
    launched_at: &str,
) -> Result<(), LaunchMetadataPersistenceError> {
    let mut first_error = state
        .record_successful_launch_metadata(
            foreground,
            instance_id.to_string(),
            launched_at.to_string(),
        )
        .await
        .err()
        .map(|_| LaunchMetadataPersistenceError::InstanceHistory);

    let username = username.to_string();
    if state
        .mutate_config(move |latest| {
            latest.username = username;
            if max_memory_mb > 0 {
                latest.max_memory_mb = max_memory_mb;
            }
            if min_memory_mb > 0 {
                latest.min_memory_mb = min_memory_mb;
            }
            Ok(())
        })
        .await
        .is_err()
        && first_error.is_none()
    {
        first_error = Some(LaunchMetadataPersistenceError::Config);
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn persist_launch_metadata_updates_instance_last_played_last_instance_and_config() {
        let root = unique_test_dir("launch-metadata-persistence");
        let state = test_app_state(&root);
        let instance = state
            .instances()
            .insert_for_test("Launch Metadata".to_string(), "1.21.1".to_string())
            .expect("add instance");
        state
            .mutate_config(move |latest| {
                latest.username = "BeforeLaunch".to_string();
                latest.max_memory_mb = 3072;
                latest.min_memory_mb = 512;
                latest.theme = "existing-theme".to_string();
                Ok(())
            })
            .await
            .expect("seed config");

        persist_launch_metadata(
            &state,
            &foreground(&state).await,
            &instance.id,
            "AfterLaunch",
            6144,
            1024,
            "2026-01-01T00:00:00.000Z",
        )
        .await
        .expect("persist launch metadata");

        let stored = state
            .instances()
            .get(&instance.id)
            .expect("stored instance");
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

    #[tokio::test]
    async fn persist_launch_metadata_keeps_existing_memory_when_new_values_are_not_positive() {
        let root = unique_test_dir("launch-metadata-non-positive-memory");
        let state = test_app_state(&root);
        let instance = state
            .instances()
            .insert_for_test("Launch Metadata".to_string(), "1.21.1".to_string())
            .expect("add instance");
        state
            .mutate_config(move |latest| {
                latest.max_memory_mb = 4096;
                latest.min_memory_mb = 768;
                Ok(())
            })
            .await
            .expect("seed config");

        persist_launch_metadata(
            &state,
            &foreground(&state).await,
            &instance.id,
            "MemoryKept",
            0,
            -1,
            "2026-01-01T00:00:00.000Z",
        )
        .await
        .expect("persist launch metadata");

        let updated = state.config().current();
        assert_eq!(updated.username, "MemoryKept");
        assert_eq!(updated.max_memory_mb, 4096);
        assert_eq!(updated.min_memory_mb, 768);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn instance_failure_does_not_skip_independent_config_metadata() {
        let root = unique_test_dir("launch-metadata-instance-failure");
        let paths = test_paths(&root);
        let state = test_app_state(&root);
        let instance = state
            .instances()
            .insert_for_test("Launch Metadata Failure".to_string(), "1.21.1".to_string())
            .expect("add instance");
        fs::create_dir_all(paths.instances_file()).expect("block instance registry path");

        let result = persist_launch_metadata(
            &state,
            &foreground(&state).await,
            &instance.id,
            "ConfigStillRuns",
            5120,
            768,
            "2026-01-01T00:00:00.000Z",
        )
        .await;

        assert_eq!(result, Err(LaunchMetadataPersistenceError::InstanceHistory));
        let stored = state
            .instances()
            .get(&instance.id)
            .expect("stored instance");
        assert!(stored.last_played_at.is_empty());
        assert_eq!(state.instances().last_instance_id(), None);
        let config = state.config().current();
        assert_eq!(config.username, "ConfigStillRuns");
        assert_eq!(config.max_memory_mb, 5120);
        assert_eq!(config.min_memory_mb, 768);

        let _ = fs::remove_dir_all(root);
    }

    async fn foreground(state: &AppState) -> IntegrityForegroundLease {
        state
            .register_integrity_foreground()
            .expect("register launch metadata foreground")
            .wait_for_settlement()
            .await
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
