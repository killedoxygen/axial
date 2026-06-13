use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use croopor_minecraft::{scan_versions, versions_dir};
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::path::{Path as FsPath, PathBuf};

const VERSION_FOLDER_OPEN_ERROR_MESSAGE: &str =
    "Could not open the version folder. Check desktop permissions and try again.";
const VERSION_DELETE_ERROR_MESSAGE: &str =
    "Could not delete the version files. Check library permissions and try again.";

#[derive(Debug, Serialize)]
struct WorldInfo {
    name: String,
    size: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    last_played: String,
}

#[derive(Debug, Serialize)]
struct SharedDataInfo {
    name: String,
    count: usize,
    size: u64,
}

#[derive(Debug, Serialize)]
struct VersionInfoResponse {
    id: String,
    folder_size: u64,
    dependents: Vec<String>,
    worlds: Vec<WorldInfo>,
    shared_data: Vec<SharedDataInfo>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/versions/{id}/info", get(handle_version_info))
        .route("/api/v1/versions/{id}", delete(handle_delete_version))
        .route(
            "/api/v1/versions/{id}/open-folder",
            post(handle_open_version_folder),
        )
}

async fn handle_version_info(
    State(state): State<AppState>,
    Path(version_id): Path<String>,
) -> Result<Json<VersionInfoResponse>, (StatusCode, Json<serde_json::Value>)> {
    let Some(mc_dir) = state.library_dir() else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        ));
    };
    if !valid_version_id(&version_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid version id" })),
        ));
    }

    let mc_dir = PathBuf::from(mc_dir);
    let version_dir = versions_dir(&mc_dir).join(&version_id);
    if !version_dir.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "version not found" })),
        ));
    }

    let all_versions = scan_versions(&mc_dir).unwrap_or_default();
    let dependents = all_versions
        .iter()
        .filter(|version| version.inherits_from == version_id)
        .map(|version| version.id.clone())
        .collect();

    Ok(Json(VersionInfoResponse {
        id: version_id,
        folder_size: dir_size(&version_dir),
        dependents,
        worlds: scan_worlds(&mc_dir.join("saves")),
        shared_data: scan_shared_data(&mc_dir),
    }))
}

async fn handle_open_version_folder(
    State(state): State<AppState>,
    Path(version_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some(mc_dir) = state.library_dir() else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        ));
    };
    if !valid_version_id(&version_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid version id" })),
        ));
    }

    let path = versions_dir(&PathBuf::from(mc_dir)).join(&version_id);
    if !path.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "version not found" })),
        ));
    }

    open_path(&path).map_err(version_folder_open_error_response)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Debug, Default, Deserialize)]
struct DeleteVersionRequest {
    #[serde(default)]
    cascade_dependents: bool,
}

async fn handle_delete_version(
    State(state): State<AppState>,
    Path(version_id): Path<String>,
    Json(payload): Json<DeleteVersionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some(mc_dir) = state.library_dir() else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        ));
    };
    if !valid_version_id(&version_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid version id" })),
        ));
    }

    let mc_dir = PathBuf::from(mc_dir);
    let version_dir = versions_dir(&mc_dir).join(&version_id);
    if !version_dir.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "version not found" })),
        ));
    }

    let mut to_delete = vec![version_id.clone()];
    if payload.cascade_dependents {
        let all_versions = scan_versions(&mc_dir).unwrap_or_default();
        to_delete.extend(
            all_versions
                .into_iter()
                .filter(|version| version.inherits_from == version_id)
                .map(|version| version.id),
        );
    }

    if let Some(running_id) = state
        .sessions()
        .first_active_version(to_delete.iter().map(String::as_str))
        .await
    {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("cannot delete version {running_id} — stop the game first")
            })),
        ));
    }

    let mut deleted = Vec::new();
    if payload.cascade_dependents {
        for id in to_delete.iter().filter(|id| *id != &version_id) {
            fs::remove_dir_all(versions_dir(&mc_dir).join(id))
                .map_err(version_delete_error_response)?;
            deleted.push(id.clone());
        }
    }

    fs::remove_dir_all(&version_dir).map_err(version_delete_error_response)?;
    deleted.push(version_id.clone());

    let affected_instances = state
        .instances()
        .list()
        .into_iter()
        .filter(|instance| deleted.iter().any(|id| id == &instance.version_id))
        .map(|instance| instance.name)
        .collect::<Vec<_>>();

    Ok(Json(serde_json::json!({
        "status": "ok",
        "deleted": deleted,
        "affected_instances": affected_instances,
    })))
}

fn valid_version_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains("..")
        && !id.contains('/')
        && !id.contains('\\')
        && FsPath::new(id) == FsPath::new(id).components().as_path()
}

fn version_folder_open_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": VERSION_FOLDER_OPEN_ERROR_MESSAGE
        })),
    )
}

fn version_delete_error_response(_error: std::io::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": VERSION_DELETE_ERROR_MESSAGE
        })),
    )
}

fn scan_worlds(saves_dir: &FsPath) -> Vec<WorldInfo> {
    fs::read_dir(saves_dir)
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let last_played = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
                .unwrap_or_default();
            WorldInfo {
                name: entry.file_name().to_string_lossy().to_string(),
                size: dir_size(&entry.path()),
                last_played,
            }
        })
        .collect()
}

fn scan_shared_data(mc_dir: &FsPath) -> Vec<SharedDataInfo> {
    ["mods", "resourcepacks", "shaderpacks"]
        .iter()
        .filter_map(|name| {
            let path = mc_dir.join(name);
            let entries = fs::read_dir(&path).ok()?;
            let items: Vec<_> = entries.filter_map(Result::ok).collect();
            if items.is_empty() {
                return None;
            }
            let size = items
                .iter()
                .filter_map(|entry| entry.metadata().ok())
                .map(|metadata| metadata.len())
                .sum();
            Some(SharedDataInfo {
                name: (*name).to_string(),
                count: items.len(),
                size,
            })
        })
        .collect()
}

fn dir_size(path: &FsPath) -> u64 {
    let mut total = 0_u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.filter_map(Result::ok) {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_dir() {
                    total += dir_size(&entry.path());
                } else {
                    total += metadata.len();
                }
            }
        }
    }
    total
}

fn open_path(path: &FsPath) -> std::io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn public_error_json(
        mapper: fn(std::io::Error) -> (StatusCode, Json<serde_json::Value>),
        internal_error: &str,
    ) -> String {
        let (status, Json(body)) = mapper(std::io::Error::other(internal_error));

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        serde_json::to_string(&body).expect("serialize public error body")
    }

    fn assert_public_error_is_bounded(
        public_json: &str,
        expected_message: &str,
        hidden_fragments: &[&str],
    ) {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(public_json)
                .expect("parse public error body")
                .get("error")
                .and_then(serde_json::Value::as_str),
            Some(expected_message)
        );

        for hidden_fragment in hidden_fragments {
            assert!(
                !public_json.contains(hidden_fragment),
                "{hidden_fragment:?} leaked in {public_json:?}"
            );
        }
    }

    #[test]
    fn version_folder_open_error_response_hides_unix_paths() {
        let public_json = public_error_json(
            version_folder_open_error_response,
            "xdg-open failed for /home/zero/.minecraft/versions/1.20.1",
        );

        assert_public_error_is_bounded(
            &public_json,
            VERSION_FOLDER_OPEN_ERROR_MESSAGE,
            &["xdg-open failed", "/home/zero", ".minecraft", "1.20.1"],
        );
    }

    #[test]
    fn version_folder_open_error_response_hides_shell_and_file_manager_text() {
        let public_json = public_error_json(
            version_folder_open_error_response,
            "gio: file:///home/zero/.minecraft/versions/1.20.1: No application is registered as handling this file",
        );

        assert_public_error_is_bounded(
            &public_json,
            VERSION_FOLDER_OPEN_ERROR_MESSAGE,
            &[
                "gio:",
                "file:///home/zero",
                "No application is registered",
                "handling this file",
            ],
        );
    }

    #[test]
    fn version_delete_error_response_hides_windows_paths() {
        let public_json = public_error_json(
            version_delete_error_response,
            r"failed to remove C:\Users\Zero\AppData\Roaming\.minecraft\versions\1.20.1",
        );

        assert_public_error_is_bounded(
            &public_json,
            VERSION_DELETE_ERROR_MESSAGE,
            &[
                r"C:\Users\Zero",
                "AppData",
                r".minecraft\versions",
                "1.20.1",
            ],
        );
    }

    #[test]
    fn version_delete_error_response_hides_raw_os_text() {
        let public_json = public_error_json(
            version_delete_error_response,
            "Permission denied (os error 13)",
        );

        assert_public_error_is_bounded(
            &public_json,
            VERSION_DELETE_ERROR_MESSAGE,
            &["Permission denied", "os error 13"],
        );
    }

    #[test]
    fn version_delete_error_response_hides_dependent_delete_details() {
        let public_json = public_error_json(
            version_delete_error_response,
            "failed to delete dependent version fabric-loader-0.15.11-1.20.1: Directory not empty (os error 39)",
        );

        assert_public_error_is_bounded(
            &public_json,
            VERSION_DELETE_ERROR_MESSAGE,
            &[
                "fabric-loader-0.15.11-1.20.1",
                "Directory not empty",
                "os error 39",
                "failed to delete dependent",
            ],
        );
    }
}
