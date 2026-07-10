//! Application-owned developer maintenance workflows.

use crate::{application::skin::clear_all_pending_saved_skin_applies, state::AppState};
use axial_config::AppConfig;
use axial_minecraft::versions_dir;
use axum::{Json, http::StatusCode};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

const DEV_LIBRARY_NOT_CONFIGURED_COPY: &str = "Axial library is not configured";
const DEV_OPERATION_FAILED_COPY: &str =
    "Developer maintenance failed. Check local app data permissions and try again.";
const DEV_BACKUP_LOCATION_COPY: &str = "local_app_data_backup";

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Serialize)]
pub struct DevCleanupResponse {
    pub status: &'static str,
    pub backup_dir: &'static str,
    pub backed_up: Vec<String>,
    pub versions_removed: usize,
    pub instances_removed: usize,
}

#[derive(Debug, Serialize)]
pub struct DevFlushResponse {
    pub status: &'static str,
    pub setup_required: bool,
    pub had_msa_auth: bool,
    pub had_accounts: bool,
    pub cleared_pending_skin_applies: usize,
}

pub async fn dev_cleanup_versions(state: &AppState) -> Result<DevCleanupResponse, ApiError> {
    let mc_dir = state
        .library_dir()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(dev_library_not_configured_error)?;

    let config_paths = state.config().paths().clone();
    let backup_dir = config_paths.config_dir.join("backups").join(format!(
        "axial-backup-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    fs::create_dir_all(&backup_dir).map_err(internal_error)?;

    let preserve = [
        "saves",
        "resourcepacks",
        "mods",
        "shaderpacks",
        "config",
        "options.txt",
        "servers.dat",
    ];
    let mut backed_up = Vec::new();
    for name in preserve {
        let src = mc_dir.join(name);
        if !src.exists() {
            continue;
        }
        let dst = backup_dir.join(name);
        copy_path(&src, &dst).map_err(internal_error)?;
        backed_up.push(name.to_string());
    }

    let instances = state.instances().list();
    let instances_removed = instances.len();
    if !instances.is_empty() {
        let backup_instances_dir = backup_dir.join("instances");
        fs::create_dir_all(&backup_instances_dir).map_err(internal_error)?;
        for inst in &instances {
            let src = state.instances().game_dir(&inst.id);
            if !src.exists() {
                continue;
            }
            let safe_name = sanitize_backup_name(&inst.name);
            let id_prefix = inst.id.chars().take(8).collect::<String>();
            let label = format!("{safe_name} ({id_prefix})");
            let dst = backup_instances_dir.join(label);
            let _ = copy_path(&src, &dst);
        }
    }

    let versions_root = versions_dir(&mc_dir);
    let mut versions_removed = 0usize;
    if let Ok(entries) = fs::read_dir(&versions_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let _ = fs::remove_dir_all(entry.path());
                versions_removed += 1;
            }
        }
    }

    let _ = fs::remove_dir_all(state.instances().paths().instances_dir.clone());
    state.instances().clear().map_err(internal_error)?;

    Ok(DevCleanupResponse {
        status: "ok",
        backup_dir: DEV_BACKUP_LOCATION_COPY,
        backed_up,
        versions_removed,
        instances_removed,
    })
}

pub async fn dev_flush(state: &AppState) -> Result<DevFlushResponse, ApiError> {
    let config_paths = state.config().paths().clone();
    let current_config = state.config().current();

    state
        .sessions()
        .terminate_all()
        .await
        .map_err(internal_error)?;
    state.installs().clear().await;
    let had_msa_auth = state
        .auth_logins()
        .clear_all()
        .await
        .map_err(internal_error)?;
    let had_accounts = state.accounts().clear_all().await.map_err(internal_error)?;
    let cleared_pending_skin_applies = clear_all_pending_saved_skin_applies().await;

    if let Some(managed_library_dir) = managed_library_dir_to_remove(&config_paths, &current_config)
    {
        let _ = fs::remove_dir_all(managed_library_dir);
    }

    let _ = fs::remove_dir_all(&config_paths.config_dir);

    state
        .config()
        .replace_in_memory(AppConfig::default())
        .map_err(internal_error)?;
    state.set_library_dir(String::new());
    state.instances().clear().map_err(internal_error)?;

    Ok(DevFlushResponse {
        status: "flushed",
        setup_required: true,
        had_msa_auth,
        had_accounts,
        cleared_pending_skin_applies,
    })
}

fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    let metadata = fs::metadata(src)?;
    if metadata.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dst)?;
    Ok(())
}

fn sanitize_backup_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect::<String>();
    if cleaned.trim().is_empty() {
        "instance".to_string()
    } else {
        cleaned
    }
}

fn dev_library_not_configured_error() -> ApiError {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({ "error": DEV_LIBRARY_NOT_CONFIGURED_COPY })),
    )
}

fn internal_error(_error: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": DEV_OPERATION_FAILED_COPY })),
    )
}

fn managed_library_dir_to_remove(
    paths: &axial_config::AppPaths,
    config: &AppConfig,
) -> Option<PathBuf> {
    let library_dir = config.library_dir.trim();
    if library_dir.is_empty() {
        return Some(paths.library_dir.clone());
    }

    if config.library_mode != "managed" {
        return None;
    }

    let candidate = PathBuf::from(library_dir);
    if candidate == paths.library_dir {
        return Some(candidate);
    }

    let config_root = normalize_for_prefix(&paths.config_dir)?;
    let library_root = normalize_for_prefix(&candidate)?;
    if library_root.starts_with(&config_root) {
        return Some(candidate);
    }

    Some(candidate)
}

fn normalize_for_prefix(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        path.canonicalize().ok()
    } else if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, AppStateInit, InstallStore, NewAuthLoginMsaToken, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn dev_flush_clears_account_state() {
        let root = test_root("auth");
        let state = test_state(&root);
        state
            .auth_logins()
            .replace_with_msa_token(NewAuthLoginMsaToken {
                access_token: "msa-access-token".to_string(),
                refresh_token: Some("msa-refresh-token".to_string()),
                id_token: None,
                token_type: "Bearer".to_string(),
                expires_in: 3600,
                scope: None,
            })
            .await;

        let response = dev_flush(&state).await.expect("dev flush should succeed");

        assert_eq!(response.status, "flushed");
        assert!(response.had_msa_auth);
        assert_eq!(state.auth_logins().active_msa_token().await, None);
        assert!(state.auth_logins().account_states().await.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dev_flush_reports_shutdown_failure_without_clearing_downstream_state() {
        let root = test_root("shutdown-failure");
        let state = test_state(&root);
        state
            .auth_logins()
            .replace_with_msa_token(NewAuthLoginMsaToken {
                access_token: "preserved-msa-access-token".to_string(),
                refresh_token: Some("preserved-msa-refresh-token".to_string()),
                id_token: None,
                token_type: "Bearer".to_string(),
                expires_in: 3600,
                scope: None,
            })
            .await;
        state.sessions().inject_rejected_process_owner().await;

        let (status, Json(body)) = match dev_flush(&state).await {
            Ok(_) => panic!("rejected process owner must fail dev flush"),
            Err(error) => error,
        };

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            serde_json::json!({ "error": DEV_OPERATION_FAILED_COPY })
        );
        assert!(state.auth_logins().active_msa_token().await.is_some());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn internal_error_uses_bounded_public_copy() {
        let root = test_root("raw-error");
        let (_, Json(body)) = internal_error(format!(
            "failed to delete {}",
            root.join("config").display()
        ));
        let public_json = serde_json::to_string(&body).expect("serialize public error");

        assert!(public_json.contains(DEV_OPERATION_FAILED_COPY));
        assert!(!public_json.contains(&root.to_string_lossy().to_string()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sanitizes_instance_backup_names() {
        assert_eq!(
            sanitize_backup_name(r#"bad/name\with:chars*?<>|"#),
            "bad_name_with_chars_____"
        );
        assert_eq!(sanitize_backup_name("   "), "instance");
    }

    fn test_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::new_with_config_dir(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "axial-api-dev-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test root");
        root
    }
}
