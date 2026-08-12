use crate::execution::physical_work;
use crate::state::AppState;
use crate::state::skins::{SavedSkinDeleteResult, SavedSkinRecord, SavedSkinStoreFailureClass};
use axial_resource::PhysicalIoClass;
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer};
use std::{collections::HashMap, sync::LazyLock};

use super::errors::{ApiError, json_error, json_status_error};

const SAVED_SKIN_NAME_MAX_CHARS: usize = 64;
const SAVED_SKIN_STORE_SCRATCH_BYTES: u64 = 32 << 20;
pub(super) const SAVED_SKIN_SOURCE: &str = "local_upload";
pub(super) const SAVED_SKIN_DEFAULT_SOURCE: &str = "minecraft_default_skin";
pub(super) const SAVED_SKIN_PROFILE_SOURCE: &str = "minecraft_profile_skin";
pub(super) const SAVED_SKIN_USERNAME_SOURCE: &str = "minecraft_username_skin";

static PENDING_SKIN_APPLIES: LazyLock<tokio::sync::Mutex<PendingSkinApplyState>> =
    LazyLock::new(|| tokio::sync::Mutex::new(PendingSkinApplyState::default()));
static PENDING_SKIN_APPLY_FLUSH_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
#[cfg(test)]
static PENDING_SKIN_APPLY_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

#[derive(Debug, Default)]
struct PendingSkinApplyState {
    pending: HashMap<String, PendingSkinApplyEntry>,
}

#[derive(Debug)]
pub(super) struct PendingSkinApplyEntry {
    pub(super) change: PendingSkinApplyChange,
    pub(super) generation: u64,
}

#[derive(Debug, Clone)]
pub(super) struct PendingSkinApplyChange {
    pub(super) login_id: String,
    pub(super) texture_key: String,
}

#[derive(Debug)]
pub(super) struct PendingSkinApplySchedule {
    pub(super) login_id: String,
    pub(super) generation: u64,
}

#[derive(Debug)]
pub(super) enum PendingSkinApplyFilter {
    Login(String),
    Generation { login_id: String, generation: u64 },
}

#[derive(Debug, Default)]
pub(super) enum CapeUpdate {
    #[default]
    Unchanged,
    Clear,
    Set(String),
}

pub(super) fn deserialize_cape_update<'de, D>(deserializer: D) -> Result<CapeUpdate, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
        .map(|value| value.map_or(CapeUpdate::Clear, CapeUpdate::Set))
}

pub(super) async fn pending_saved_skin_apply_texture_key_for_active_account(
    state: &AppState,
) -> Option<String> {
    let login_id = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await?
        .account
        .login_id;
    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .get(&login_id)
        .map(|entry| entry.change.texture_key.clone())
}

pub(super) async fn insert_pending_saved_skin_apply(
    change: PendingSkinApplyChange,
) -> PendingSkinApplySchedule {
    let login_id = change.login_id.clone();
    let generation = {
        let mut pending = PENDING_SKIN_APPLIES.lock().await;
        let generation = pending
            .pending
            .get(&login_id)
            .map_or(1, |entry| entry.generation.wrapping_add(1));
        pending.pending.insert(
            login_id.clone(),
            PendingSkinApplyEntry { change, generation },
        );
        generation
    };

    PendingSkinApplySchedule {
        login_id,
        generation,
    }
}

pub(super) async fn retarget_pending_saved_skin_apply(
    old_texture_key: &str,
    new_texture_key: &str,
) {
    let mut pending = PENDING_SKIN_APPLIES.lock().await;
    for entry in pending.pending.values_mut() {
        if entry.change.texture_key == old_texture_key {
            entry.change.texture_key = new_texture_key.to_string();
        }
    }
}

pub(crate) async fn clear_pending_saved_skin_apply_for_login_id(login_id: &str) -> bool {
    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .remove(login_id)
        .is_some()
}

pub(crate) async fn clear_all_pending_saved_skin_applies() -> usize {
    let mut pending = PENDING_SKIN_APPLIES.lock().await;
    let cleared = pending.pending.len();
    pending.pending.clear();
    cleared
}

#[cfg(test)]
pub(crate) async fn test_set_pending_saved_skin_apply_for_login_id(login_id: &str) {
    let mut pending = PENDING_SKIN_APPLIES.lock().await;
    pending.pending.insert(
        login_id.to_string(),
        PendingSkinApplyEntry {
            change: PendingSkinApplyChange {
                login_id: login_id.to_string(),
                texture_key: format!("test-texture-{login_id}"),
            },
            generation: 1,
        },
    );
}

pub(super) async fn clear_pending_saved_skin_apply_for_active_account(state: &AppState) -> bool {
    let Some(account_state) = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
    else {
        return false;
    };

    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .remove(&account_state.account.login_id)
        .is_some()
}

pub(super) async fn clear_pending_saved_skin_apply_for_texture(texture_key: &str) {
    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .retain(|_, entry| entry.change.texture_key != texture_key);
}

pub(super) async fn pending_saved_skin_apply_flush_guard() -> tokio::sync::MutexGuard<'static, ()> {
    PENDING_SKIN_APPLY_FLUSH_LOCK.lock().await
}

#[cfg(test)]
pub(crate) async fn pending_saved_skin_apply_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    PENDING_SKIN_APPLY_TEST_LOCK.lock().await
}

pub(super) async fn take_pending_saved_skin_apply(
    filter: &PendingSkinApplyFilter,
) -> Option<PendingSkinApplyEntry> {
    let mut pending = PENDING_SKIN_APPLIES.lock().await;
    match filter {
        PendingSkinApplyFilter::Login(login_id) => pending.pending.remove(login_id),
        PendingSkinApplyFilter::Generation {
            login_id,
            generation,
        } => {
            let entry = pending.pending.get(login_id)?;
            if entry.generation != *generation {
                return None;
            }
            pending.pending.remove(login_id)
        }
    }
}

pub(super) async fn restore_pending_saved_skin_apply(
    entry: PendingSkinApplyEntry,
) -> PendingSkinApplySchedule {
    let login_id = entry.change.login_id.clone();
    let generation = entry.generation;
    let mut pending = PENDING_SKIN_APPLIES.lock().await;
    pending.pending.entry(login_id.clone()).or_insert(entry);
    PendingSkinApplySchedule {
        login_id,
        generation,
    }
}

pub(super) async fn list_saved_skins(state: &AppState) -> Result<Vec<SavedSkinRecord>, ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Read, move || skins.list())
        .await
        .map_err(skin_read_error)
}

pub(super) async fn save_saved_skin(
    state: &AppState,
    texture_key: String,
    name: String,
    variant: String,
    source: String,
    cape_id: Option<String>,
    png_bytes: Vec<u8>,
) -> Result<SavedSkinRecord, ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Heavy, move || {
        skins.save(texture_key, name, variant, source, cape_id, &png_bytes)
    })
    .await
    .map_err(skin_write_error)
}

pub(super) async fn delete_saved_skin(
    state: &AppState,
    texture_key: String,
) -> Result<SavedSkinDeleteResult, ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Heavy, move || {
        skins.delete_unapplied(&texture_key)
    })
    .await
    .map_err(skin_write_error)
}

pub(super) async fn update_saved_skin_metadata(
    state: &AppState,
    texture_key: String,
    name: Option<String>,
    variant: Option<String>,
    cape_id: Option<Option<String>>,
) -> Result<Option<SavedSkinRecord>, ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Heavy, move || {
        skins.update_metadata(&texture_key, name, variant, cape_id)
    })
    .await
    .map_err(skin_write_error)
}

pub(super) async fn replace_saved_skin_texture(
    state: &AppState,
    texture_key: String,
    new_texture_key: String,
    name: String,
    variant: String,
    cape_id: Option<String>,
    png_bytes: Vec<u8>,
) -> Result<Option<SavedSkinRecord>, ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Heavy, move || {
        skins.replace_texture(
            &texture_key,
            new_texture_key,
            name,
            variant,
            cape_id,
            &png_bytes,
        )
    })
    .await
    .map_err(skin_write_error)
}

pub(super) async fn read_saved_skin_png(
    state: &AppState,
    texture_key: String,
) -> Result<Option<Vec<u8>>, ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Read, move || skins.read_png(&texture_key))
        .await
        .map_err(skin_read_error)
}

pub(super) async fn mark_saved_skin_applied(
    state: &AppState,
    texture_key: String,
) -> Result<Option<String>, ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Heavy, move || {
        skins.mark_applied(&texture_key)
    })
    .await
    .map_err(skin_write_error)
}

pub(super) async fn clear_saved_skin_applied(state: &AppState) -> Result<(), ApiError> {
    let skins = state.skins().clone();
    run_saved_skin_store(PhysicalIoClass::Heavy, move || skins.clear_applied())
        .await
        .map_err(skin_write_error)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SavedSkinStoreFailure {
    TaskStopped,
    Storage(SavedSkinStoreFailureClass),
}

async fn run_saved_skin_store<T, Work>(
    io: PhysicalIoClass,
    work: Work,
) -> Result<T, SavedSkinStoreFailure>
where
    T: Send + 'static,
    Work: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    physical_work::run(io, SAVED_SKIN_STORE_SCRATCH_BYTES, work)
        .await
        .map_err(|_| SavedSkinStoreFailure::TaskStopped)?
        .map_err(|error| {
            SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::from_error(&error))
        })
}

pub(super) fn validate_saved_skin_name(value: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "skin name is required"));
    }
    if trimmed.chars().count() > SAVED_SKIN_NAME_MAX_CHARS {
        return Err(json_error(StatusCode::BAD_REQUEST, "skin name is too long"));
    }
    if trimmed
        .chars()
        .any(|value| value.is_control() || matches!(value, '/' | '\\'))
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin name contains unsupported characters",
        ));
    }

    Ok(trimmed.to_string())
}

pub(super) fn default_profile_skin_name(profile_name: &str) -> String {
    format!("{} profile skin", profile_name.trim())
        .chars()
        .take(SAVED_SKIN_NAME_MAX_CHARS)
        .collect()
}

pub(super) fn default_username_skin_name(profile_name: &str) -> String {
    format!("{} skin", profile_name.trim())
        .chars()
        .take(SAVED_SKIN_NAME_MAX_CHARS)
        .collect()
}

pub(super) fn validate_saved_skin_upload_source(value: Option<&str>) -> Result<String, ApiError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(SAVED_SKIN_SOURCE.to_string());
    };
    if value == SAVED_SKIN_DEFAULT_SOURCE {
        return Ok(SAVED_SKIN_DEFAULT_SOURCE.to_string());
    }

    Err(json_error(
        StatusCode::BAD_REQUEST,
        "skin source is not supported for uploads",
    ))
}

pub(super) fn validate_saved_skin_variant(value: Option<&str>) -> Result<String, ApiError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok("classic".to_string());
    };
    if value.eq_ignore_ascii_case("classic") {
        return Ok("classic".to_string());
    }
    if value.eq_ignore_ascii_case("slim") {
        return Ok("slim".to_string());
    }

    Err(json_error(
        StatusCode::BAD_REQUEST,
        "skin variant must be classic or slim",
    ))
}

pub(super) async fn validate_saved_skin_cape_update(
    state: &AppState,
    value: &CapeUpdate,
) -> Result<Option<Option<String>>, ApiError> {
    let cape_id = match value {
        CapeUpdate::Unchanged => return Ok(None),
        CapeUpdate::Clear => return Ok(Some(None)),
        CapeUpdate::Set(cape_id) => cape_id,
    };
    let cape_id = validate_saved_skin_cape_id(cape_id)?;
    let minecraft_state = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::CONFLICT,
                "Minecraft account is required to select a cape",
                "minecraft_account_required",
            )
        })?;
    if !minecraft_state
        .account
        .profile
        .capes
        .iter()
        .any(|cape| cape.id == cape_id)
    {
        return Err(json_status_error(
            StatusCode::BAD_REQUEST,
            "Minecraft cape is not available for this account",
            "minecraft_cape_unavailable",
        ));
    }

    Ok(Some(Some(cape_id)))
}

pub(super) fn validate_saved_skin_cape_id(value: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 80 {
        return Err(json_error(StatusCode::BAD_REQUEST, "invalid cape id"));
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(json_error(StatusCode::BAD_REQUEST, "invalid cape id"));
    }

    Ok(trimmed.to_string())
}

pub(super) fn validate_texture_key(value: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.len() != 64
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(json_error(StatusCode::BAD_REQUEST, "invalid texture key"));
    }

    Ok(trimmed.to_string())
}

fn skin_read_error(error: SavedSkinStoreFailure) -> ApiError {
    skin_store_error(error, "read")
}

fn skin_write_error(error: SavedSkinStoreFailure) -> ApiError {
    skin_store_error(error, "update")
}

fn skin_store_error(error: SavedSkinStoreFailure, operation: &'static str) -> ApiError {
    let (status, message) = match error {
        SavedSkinStoreFailure::TaskStopped => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Saved skin storage stopped before the operation completed. Try again.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::PermissionDenied) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Saved skin storage access was denied. Check app data permissions and try again.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::StorageFull) => (
            StatusCode::INSUFFICIENT_STORAGE,
            "Saved skin storage is full. Free disk space and try again.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::NotFound) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Saved skin storage is incomplete. Restart Axial and try again.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Conflict) => (
            StatusCode::CONFLICT,
            "Another saved skin update conflicts with this operation. Try again.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Interrupted) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The saved skin operation was interrupted. Try again.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Unsettled) => (
            StatusCode::CONFLICT,
            "Another saved skin update is still settling. Try again.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::InvalidData) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Saved skin data is invalid and could not be loaded safely.",
        ),
        SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Other) => {
            let message = if operation == "read" {
                "Could not read saved skins. Try again."
            } else {
                "Could not update saved skins. Try again."
            };
            (StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    };
    json_error(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;

    #[test]
    fn p02_b05_contract_cross_owner_saved_skin_failures_have_distinct_public_classes() {
        let cases = [
            (
                SavedSkinStoreFailure::TaskStopped,
                StatusCode::SERVICE_UNAVAILABLE,
                "Saved skin storage stopped before the operation completed. Try again.",
            ),
            (
                SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::PermissionDenied),
                StatusCode::INTERNAL_SERVER_ERROR,
                "Saved skin storage access was denied. Check app data permissions and try again.",
            ),
            (
                SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::StorageFull),
                StatusCode::INSUFFICIENT_STORAGE,
                "Saved skin storage is full. Free disk space and try again.",
            ),
            (
                SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::NotFound),
                StatusCode::INTERNAL_SERVER_ERROR,
                "Saved skin storage is incomplete. Restart Axial and try again.",
            ),
            (
                SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Conflict),
                StatusCode::CONFLICT,
                "Another saved skin update conflicts with this operation. Try again.",
            ),
            (
                SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Interrupted),
                StatusCode::SERVICE_UNAVAILABLE,
                "The saved skin operation was interrupted. Try again.",
            ),
            (
                SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Unsettled),
                StatusCode::CONFLICT,
                "Another saved skin update is still settling. Try again.",
            ),
            (
                SavedSkinStoreFailure::Storage(SavedSkinStoreFailureClass::Other),
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not update saved skins. Try again.",
            ),
        ];

        for (error, expected_status, expected_message) in cases {
            let (status, Json(body)) = skin_write_error(error);
            assert_eq!(status, expected_status);
            assert_eq!(body["error"], expected_message);
        }
    }
}
