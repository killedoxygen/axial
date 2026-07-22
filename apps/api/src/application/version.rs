use crate::{
    application::filesystem::{
        BlockingFilesystemTaskError, FilesystemEntryKind, FilesystemScanBudget,
        FilesystemScanError, FilesystemScanLimits, admit_exclusive_blocking_filesystem,
        run_blocking_filesystem,
    },
    state::{AppState, InstalledVersionsSnapshot, ProducerLease},
};
use axial_minecraft::{
    LifecycleMeta, MinecraftVersionMeta, VersionEntry, VersionScanReport, VersionScanState,
    VersionSubjectKind, analyze_minecraft_version, enrich_version_entries,
    fetch_version_manifest_cached, managed_path::ManagedLibraryOperation,
    manifest_release_references, versions_dir,
};
use axum::{Json, http::StatusCode};
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path as FsPath, PathBuf};

const VERSION_FOLDER_OPEN_ERROR_MESSAGE: &str =
    "Could not open the version folder. Check desktop permissions and try again.";
const VERSION_DELETE_ERROR_MESSAGE: &str =
    "Could not delete the version files. Check library permissions and try again.";
pub(crate) const VERSION_SCAN_DEGRADED_MESSAGE: &str =
    "Could not verify installed versions. Check the library folder and try again.";
const VERSION_INFO_SCAN_LIMITS: FilesystemScanLimits = FilesystemScanLimits {
    max_depth: 32,
    max_entries: 100_000,
    max_bytes: 1024 * 1024 * 1024 * 1024,
};

#[derive(Debug, Serialize)]
pub struct VersionsResponse {
    pub versions: Vec<VersionEntry>,
    pub scan_state: VersionScanViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VersionScanViewModel {
    pub state_id: String,
    pub label: String,
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct InstalledVersionsScan {
    pub versions: Vec<VersionEntry>,
    pub view_model: VersionScanViewModel,
}

impl InstalledVersionsScan {
    pub(crate) fn is_degraded(&self) -> bool {
        self.view_model.degraded
    }
}

#[derive(Debug, Serialize)]
pub struct CatalogEntry {
    pub subject_kind: VersionSubjectKind,
    pub id: String,
    pub raw_kind: String,
    pub release_time: String,
    pub minecraft_meta: MinecraftVersionMeta,
    pub lifecycle: LifecycleMeta,
    pub url: String,
    pub installed: bool,
}

#[derive(Debug, Serialize)]
pub struct CatalogResponse {
    pub latest: axial_minecraft::manifest::LatestVersions,
    pub versions: Vec<CatalogEntry>,
}

#[derive(Debug, Serialize)]
pub struct WorldInfo {
    pub name: String,
    pub size: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_played: String,
}

#[derive(Debug, Serialize)]
pub struct SharedDataInfo {
    pub name: String,
    pub count: usize,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct VersionInfoResponse {
    pub id: String,
    pub folder_size: u64,
    pub dependents: Vec<String>,
    pub worlds: Vec<WorldInfo>,
    pub shared_data: Vec<SharedDataInfo>,
}

pub(crate) async fn installed_versions(
    state: &AppState,
    producer: &ProducerLease,
) -> Result<VersionsResponse, (StatusCode, Json<serde_json::Value>)> {
    let snapshot = state
        .installed_versions_snapshot(producer)
        .await
        .ok_or_else(version_library_not_configured_response)?;
    let mut scan = installed_versions_scan(&snapshot.snapshot);
    enrich_versions_from_cached_manifest(snapshot.managed_library_operation(), &mut scan.versions)
        .await;

    Ok(VersionsResponse {
        versions: scan.versions,
        scan_state: scan.view_model,
    })
}

pub(crate) async fn installed_versions_event_payload(
    state: &AppState,
    producer: &ProducerLease,
) -> String {
    match installed_versions(state, producer).await {
        Ok(response) => {
            serde_json::to_string(&response).unwrap_or_else(|_| "{\"versions\":[]}".to_string())
        }
        Err(_) => "{\"versions\":[]}".to_string(),
    }
}

pub(crate) async fn catalog(
    state: &AppState,
    producer: &ProducerLease,
) -> Result<CatalogResponse, (StatusCode, Json<serde_json::Value>)> {
    let snapshot = state
        .installed_versions_snapshot(producer)
        .await
        .ok_or_else(version_library_not_configured_response)?;
    let manifest = fetch_version_manifest_cached(snapshot.managed_library_operation())
        .await
        .map_err(catalog_fetch_error_response)?;

    let installed: HashSet<String> = snapshot
        .snapshot
        .report()
        .versions
        .iter()
        .filter(|version| version.launchable)
        .map(|version| version.id.clone())
        .collect();

    let releases = manifest_release_references(&manifest);
    let versions = manifest
        .versions
        .into_iter()
        .map(|version| {
            let analysis = analyze_minecraft_version(
                &version.id,
                &version.kind,
                &version.release_time,
                None,
                &releases,
            );
            CatalogEntry {
                subject_kind: VersionSubjectKind::MinecraftVersion,
                installed: installed.contains(&version.id),
                id: version.id,
                raw_kind: version.kind,
                release_time: version.release_time,
                minecraft_meta: analysis.minecraft_meta,
                lifecycle: analysis.lifecycle,
                url: version.url,
            }
        })
        .collect();

    Ok(CatalogResponse {
        latest: manifest.latest,
        versions,
    })
}

pub(crate) async fn version_info(
    state: &AppState,
    producer: &ProducerLease,
    version_id: &str,
) -> Result<VersionInfoResponse, (StatusCode, Json<serde_json::Value>)> {
    let snapshot = state
        .installed_versions_snapshot(producer)
        .await
        .ok_or_else(version_library_not_configured_response)?;
    let mc_dir = snapshot.library_dir().to_path_buf();
    if !valid_version_id(version_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid version id" })),
        ));
    }

    let scan = installed_versions_scan(&snapshot.snapshot);
    let scan_degraded = scan.is_degraded();
    let all_versions = scan.versions;
    let dependents = all_versions
        .iter()
        .filter(|version| version.inherits_from == version_id)
        .map(|version| version.id.clone())
        .collect();

    let response_id = version_id.to_string();
    let version_dir = versions_dir(&mc_dir).join(version_id);
    run_blocking_filesystem(move || {
        match fs::symlink_metadata(&version_dir) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(VersionInfoFilesystemError::Scan(FilesystemScanError::Link));
            }
            Ok(_) => return Err(VersionInfoFilesystemError::VersionNotFound),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(VersionInfoFilesystemError::VersionNotFound);
            }
            Err(error) => {
                return Err(VersionInfoFilesystemError::Scan(FilesystemScanError::Io(
                    error,
                )));
            }
        }
        if scan_degraded {
            return Err(VersionInfoFilesystemError::ScanDegraded);
        }
        let mut budget = FilesystemScanBudget::new(VERSION_INFO_SCAN_LIMITS);
        let folder_size = budget
            .directory_size(&version_dir)
            .map_err(VersionInfoFilesystemError::Scan)?;
        let worlds = scan_worlds(&mc_dir.join("saves"), &mut budget)
            .map_err(VersionInfoFilesystemError::Scan)?;
        let shared_data =
            scan_shared_data(&mc_dir, &mut budget).map_err(VersionInfoFilesystemError::Scan)?;
        Ok::<_, VersionInfoFilesystemError>(VersionInfoResponse {
            id: response_id,
            folder_size,
            dependents,
            worlds,
            shared_data,
        })
    })
    .await
    .map_err(version_info_task_error_response)?
    .map_err(version_info_scan_error_response)
}

pub fn open_version_folder(
    state: &AppState,
    version_id: &str,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let mc_dir = version_library_dir(state)?;
    if !valid_version_id(version_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid version id" })),
        ));
    }

    let path = versions_dir(&mc_dir).join(version_id);
    if !path.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "version not found" })),
        ));
    }

    open_path(&path).map_err(version_folder_open_error_response)?;

    Ok(serde_json::json!({ "status": "ok" }))
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteVersionRequest {
    #[serde(default)]
    pub cascade_dependents: bool,
}

pub(crate) async fn delete_version(
    state: &AppState,
    producer: &ProducerLease,
    version_id: &str,
    payload: DeleteVersionRequest,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let snapshot = state
        .installed_versions_snapshot(producer)
        .await
        .ok_or_else(version_library_not_configured_response)?;
    let mc_dir = snapshot.library_dir().to_path_buf();
    if !valid_version_id(version_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid version id" })),
        ));
    }

    let version_dir = versions_dir(&mc_dir).join(version_id);
    if !version_dir_is_regular_directory(&version_dir) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "version not found" })),
        ));
    }

    let mut to_delete = vec![version_id.to_string()];
    if payload.cascade_dependents {
        let scan = installed_versions_scan(&snapshot.snapshot);
        if scan.is_degraded() {
            return Err(version_scan_degraded_response());
        }
        let all_versions = scan.versions;
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
                "error": format!("cannot delete version {running_id}; stop the game first")
            })),
        ));
    }

    let filesystem = admit_exclusive_blocking_filesystem()
        .await
        .map_err(version_delete_task_error_response)?;
    let mutation = state
        .admit_managed_artifact_mutation()
        .map_err(|error| version_delete_error_response(std::io::Error::other(error.to_string())))?;
    let state_for_delete = state.clone();
    let primary_version_id = version_id.to_string();
    let deleted = filesystem
        .run(move || {
            let _mutation = mutation;
            let mut deleted = Vec::new();
            if payload.cascade_dependents {
                for id in to_delete
                    .iter()
                    .filter(|id| id.as_str() != primary_version_id)
                {
                    remove_version_dir(&state_for_delete, versions_dir(&mc_dir).join(id))?;
                    deleted.push(id.clone());
                }
            }
            remove_version_dir(&state_for_delete, version_dir)?;
            deleted.push(primary_version_id);
            Ok::<_, (StatusCode, Json<serde_json::Value>)>(deleted)
        })
        .await
        .map_err(version_delete_task_error_response)??;

    let affected_instances = state
        .instances()
        .list()
        .into_iter()
        .filter(|instance| deleted.iter().any(|id| id == &instance.version_id))
        .map(|instance| instance.name)
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "status": "ok",
        "deleted": deleted,
        "affected_instances": affected_instances,
    }))
}

fn version_library_dir(state: &AppState) -> Result<PathBuf, (StatusCode, Json<serde_json::Value>)> {
    let Some(mc_dir) = state.library_dir() else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Axial library is not configured" })),
        ));
    };
    Ok(PathBuf::from(mc_dir))
}

fn version_library_not_configured_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({ "error": "Axial library is not configured" })),
    )
}

fn remove_version_dir(
    state: &AppState,
    path: PathBuf,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let result = fs::remove_dir_all(path).map_err(version_delete_error_response);
    state.invalidate_installed_versions();
    result
}

pub(crate) fn installed_versions_scan(
    snapshot: &InstalledVersionsSnapshot,
) -> InstalledVersionsScan {
    let report = snapshot.report();
    InstalledVersionsScan {
        view_model: version_scan_view_model(report),
        versions: report.versions.clone(),
    }
}

pub(crate) fn version_scan_degraded_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({ "error": VERSION_SCAN_DEGRADED_MESSAGE })),
    )
}

fn version_scan_view_model(report: &VersionScanReport) -> VersionScanViewModel {
    match report.state {
        VersionScanState::Ready => VersionScanViewModel {
            state_id: "ready".to_string(),
            label: "Installed versions ready".to_string(),
            degraded: false,
            detail: None,
        },
        VersionScanState::Empty => VersionScanViewModel {
            state_id: "empty".to_string(),
            label: "No installed versions".to_string(),
            degraded: false,
            detail: None,
        },
        VersionScanState::Degraded => VersionScanViewModel {
            state_id: "degraded".to_string(),
            label: "Installed versions unavailable".to_string(),
            degraded: true,
            detail: Some(VERSION_SCAN_DEGRADED_MESSAGE.to_string()),
        },
    }
}

async fn enrich_versions_from_cached_manifest(
    operation: &ManagedLibraryOperation,
    versions: &mut [VersionEntry],
) {
    if let Ok(manifest) = fetch_version_manifest_cached(operation).await {
        let releases = manifest_release_references(&manifest);
        enrich_version_entries(versions, &releases);
    }
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

fn catalog_fetch_error_response(
    _error: impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({
            "error": "Could not load the Minecraft catalog. Check your connection and try again."
        })),
    )
}

fn scan_worlds(
    saves_dir: &FsPath,
    budget: &mut FilesystemScanBudget,
) -> Result<Vec<WorldInfo>, FilesystemScanError> {
    let mut worlds = Vec::new();
    for entry in budget.read_optional_directory(saves_dir)? {
        if entry.kind != FilesystemEntryKind::Directory {
            continue;
        }
        let last_played = entry
            .metadata
            .modified()
            .ok()
            .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
            .unwrap_or_default();
        worlds.push(WorldInfo {
            name: entry.name.to_string_lossy().into_owned(),
            size: budget.directory_size(&entry.path)?,
            last_played,
        });
    }
    Ok(worlds)
}

fn scan_shared_data(
    mc_dir: &FsPath,
    budget: &mut FilesystemScanBudget,
) -> Result<Vec<SharedDataInfo>, FilesystemScanError> {
    let mut shared_data = Vec::new();
    for name in ["mods", "resourcepacks", "shaderpacks"] {
        let entries = budget.read_optional_directory(&mc_dir.join(name))?;
        if entries.is_empty() {
            continue;
        }
        let mut size = 0_u64;
        for entry in &entries {
            let entry_size = match entry.kind {
                FilesystemEntryKind::Directory => budget.directory_size(&entry.path)?,
                FilesystemEntryKind::File => {
                    budget.account_file_bytes(entry.metadata.len())?;
                    entry.metadata.len()
                }
            };
            size = size
                .checked_add(entry_size)
                .ok_or(FilesystemScanError::ByteLimit)?;
        }
        shared_data.push(SharedDataInfo {
            name: name.to_string(),
            count: entries.len(),
            size,
        });
    }
    Ok(shared_data)
}

fn version_dir_is_regular_directory(path: &FsPath) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

#[derive(Debug)]
enum VersionInfoFilesystemError {
    VersionNotFound,
    ScanDegraded,
    Scan(FilesystemScanError),
}

fn version_info_scan_error_response(
    error: VersionInfoFilesystemError,
) -> (StatusCode, Json<serde_json::Value>) {
    let error = match error {
        VersionInfoFilesystemError::VersionNotFound => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "version not found" })),
            );
        }
        VersionInfoFilesystemError::ScanDegraded => return version_scan_degraded_response(),
        VersionInfoFilesystemError::Scan(error) => error,
    };
    if error.is_capacity_limit() {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "error": "version information exceeds safe scan limits"
            })),
        );
    }
    if error.is_unsupported_layout() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "version information contains unsupported filesystem entries"
            })),
        );
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not read version information. Check library permissions and try again."
        })),
    )
}

fn version_info_task_error_response(
    _error: BlockingFilesystemTaskError,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not complete the version information scan. Try again."
        })),
    )
}

fn version_delete_task_error_response(
    _error: BlockingFilesystemTaskError,
) -> (StatusCode, Json<serde_json::Value>) {
    version_delete_error_response(std::io::Error::other("version delete task failed"))
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

    #[test]
    fn installed_version_scan_view_model_marks_malformed_library_as_degraded() {
        let report = VersionScanReport {
            state: VersionScanState::Degraded,
            versions: Vec::new(),
            issues: Vec::new(),
        };
        let view_model = version_scan_view_model(&report);

        assert!(view_model.degraded);
        assert_eq!(view_model.state_id, "degraded");
        assert_eq!(
            view_model.detail.as_deref(),
            Some(VERSION_SCAN_DEGRADED_MESSAGE)
        );
    }

    #[test]
    fn catalog_fetch_error_is_bad_gateway_with_bounded_copy() {
        let (status, Json(body)) = catalog_fetch_error_response(
            "request failed for https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
        );

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body["error"],
            "Could not load the Minecraft catalog. Check your connection and try again."
        );
    }

    #[test]
    fn catalog_fetch_error_does_not_expose_upstream_details() {
        let fragments = [
            "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
            "error sending request for url",
            "expected value at line 1 column 1",
        ];

        for fragment in fragments {
            let (_status, Json(body)) = catalog_fetch_error_response(format!(
                "failed to fetch version manifest: {fragment}"
            ));
            let rendered = body.to_string();

            assert!(
                !rendered.contains(fragment),
                "public response exposed upstream detail: {fragment}"
            );
        }
    }

    #[test]
    fn bounded_filesystem_version_info_errors_distinguish_root_layout_and_capacity() {
        let cases = [
            (
                VersionInfoFilesystemError::VersionNotFound,
                StatusCode::NOT_FOUND,
                "version not found",
            ),
            (
                VersionInfoFilesystemError::Scan(FilesystemScanError::Link),
                StatusCode::UNPROCESSABLE_ENTITY,
                "version information contains unsupported filesystem entries",
            ),
            (
                VersionInfoFilesystemError::Scan(FilesystemScanError::EntryLimit),
                StatusCode::PAYLOAD_TOO_LARGE,
                "version information exceeds safe scan limits",
            ),
        ];

        for (error, expected_status, expected_message) in cases {
            let (status, Json(body)) = version_info_scan_error_response(error);
            assert_eq!(status, expected_status);
            assert_eq!(body["error"], expected_message);
        }
    }
}
