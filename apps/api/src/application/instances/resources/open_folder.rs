use crate::state::AppState;
use axum::{Json, http::StatusCode};
use serde::Deserialize;
use std::path::{Path as FsPath, PathBuf};

const INSTANCE_SUBFOLDERS: [&str; 7] = [
    "mods",
    "saves",
    "resourcepacks",
    "shaderpacks",
    "config",
    "screenshots",
    "logs",
];

#[derive(Debug, Deserialize)]
pub(crate) struct OpenFolderQuery {
    pub sub: Option<String>,
}

pub(crate) async fn handle_open_instance_folder(
    state: &AppState,
    id: &str,
    query: OpenFolderQuery,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;

    let game_dir = state.instances().game_dir(&instance.id);
    let dir = resolve_instance_folder(&game_dir, query.sub.as_deref()).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
    })?;

    std::fs::create_dir_all(&dir).map_err(instance_folder_prepare_error_response)?;
    open_path(&dir).map_err(instance_folder_open_error_response)?;

    Ok(serde_json::json!({ "status": "ok" }))
}

pub(in crate::application::instances) fn instance_folder_prepare_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not prepare the instance folder. Check app data permissions and try again."
        })),
    )
}

pub(in crate::application::instances) fn instance_folder_open_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not open the instance folder. Check desktop permissions and try again."
        })),
    )
}

pub(in crate::application::instances) fn resolve_instance_folder(
    game_dir: &FsPath,
    sub: Option<&str>,
) -> Result<PathBuf, &'static str> {
    match sub {
        None => Ok(game_dir.to_path_buf()),
        Some(subfolder) if INSTANCE_SUBFOLDERS.contains(&subfolder) => Ok(game_dir.join(subfolder)),
        Some(_) => Err("invalid instance folder"),
    }
}

fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(path);
        cmd
    } else if cfg!(target_os = "macos") {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(path);
        cmd
    } else {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(path);
        cmd
    };

    let _child = command.spawn()?;
    Ok(())
}
