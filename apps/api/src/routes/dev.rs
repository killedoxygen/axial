use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use croopor_config::AppConfig;
use croopor_minecraft::versions_dir;
use std::fs;
use std::path::{Path, PathBuf};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/dev/cleanup-versions", post(handle_dev_cleanup))
        .route("/api/v1/dev/flush", post(handle_dev_flush))
}

async fn handle_dev_cleanup(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mc_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let mc_dir = PathBuf::from(mc_dir);

    let config_paths = state.config().paths().clone();
    let backup_dir = config_paths.config_dir.join("backups").join(format!(
        "croopor-backup-{}",
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
            let label = format!("{} ({})", safe_name, &inst.id[..inst.id.len().min(8)]);
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

    Ok(Json(serde_json::json!({
        "status": "ok",
        "backup_dir": backup_dir.to_string_lossy(),
        "backed_up": backed_up,
        "versions_removed": versions_removed,
        "instances_removed": instances_removed,
    })))
}

async fn handle_dev_flush(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let config_paths = state.config().paths().clone();
    let current_config = state.config().current();

    state.sessions().terminate_all().await;
    state.installs().clear().await;

    if let Some(managed_library_dir) = managed_library_dir_to_remove(&config_paths, &current_config)
    {
        let _ = fs::remove_dir_all(managed_library_dir);
    }

    let _ = fs::remove_dir_all(&config_paths.config_dir);

    state.config().replace_in_memory(AppConfig::default());
    state.set_library_dir(String::new());
    let _ = state.instances().clear();

    Ok(Json(serde_json::json!({
        "status": "flushed",
        "setup_required": true
    })))
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

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
}

fn managed_library_dir_to_remove(
    paths: &croopor_config::AppPaths,
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
