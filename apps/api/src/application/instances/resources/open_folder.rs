use super::acquire_instance_resource_lifecycle;
use crate::{
    application::{
        filesystem::admit_blocking_filesystem,
        platform_opener::{
            NativeFolderOpenError, NativeFolderProjection, open_native_folder_owned,
        },
    },
    state::{AppState, RequestProducerHandoff},
};
use axum::{Json, http::StatusCode};
use serde::Deserialize;
#[cfg(test)]
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
    handoff: RequestProducerHandoff,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    state.instances().get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let sub = query.sub;
    validate_instance_folder(sub.as_deref()).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
    })?;
    let filesystem = admit_blocking_filesystem().await.map_err(|_| {
        instance_folder_prepare_error_response(std::io::Error::other(
            "filesystem admission refused",
        ))
    })?;
    let lifecycle = acquire_instance_resource_lifecycle(state, id).await?;
    let content = state
        .admit_instance_content_authority(lifecycle)
        .await
        .map_err(instance_folder_prepare_error_response)?;
    let producer = handoff.try_claim().map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Application shutdown is in progress. Try opening the folder again."
            })),
        )
    })?;
    let shutdown = state.subscribe_shutdown();
    open_native_folder_owned(filesystem, producer, shutdown, move || {
        let mut folder = content.activate()?.prepare_native_folder(sub.as_deref())?;
        let path = folder.path().to_path_buf();
        Ok(NativeFolderProjection::new(path, move || {
            folder.revalidate()
        }))
    })
    .await
    .map_err(native_folder_error_response)?;

    Ok(serde_json::json!({ "status": "ok" }))
}

fn native_folder_error_response(
    error: NativeFolderOpenError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        NativeFolderOpenError::Capacity => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Too many folder windows are still opening. Try again shortly."
            })),
        ),
        NativeFolderOpenError::PhysicalTask | NativeFolderOpenError::Prepare => {
            instance_folder_prepare_error_response(std::io::Error::other(
                "native folder preparation failed",
            ))
        }
        NativeFolderOpenError::Spawn => instance_folder_open_error_response(std::io::Error::other(
            "native folder opener failed",
        )),
    }
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

#[cfg(test)]
pub(in crate::application::instances) fn resolve_instance_folder(
    game_dir: &FsPath,
    sub: Option<&str>,
) -> Result<PathBuf, &'static str> {
    validate_instance_folder(sub)?;
    match sub {
        None => Ok(game_dir.to_path_buf()),
        Some(subfolder) => Ok(game_dir.join(subfolder)),
    }
}

fn validate_instance_folder(sub: Option<&str>) -> Result<(), &'static str> {
    match sub {
        None => Ok(()),
        Some(subfolder) if INSTANCE_SUBFOLDERS.contains(&subfolder) => Ok(()),
        Some(_) => Err("invalid instance folder"),
    }
}
