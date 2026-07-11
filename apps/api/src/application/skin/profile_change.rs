use crate::state::skins::SavedSkinRecord;
use crate::state::{AppState, AuthLoginMinecraftAccount, ProducerLease, RequestProducerHandoff};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::cache::{
    profile_skin_file_cache_path, read_profile_skin_file_cache, write_profile_file_cache,
};
use super::errors::{
    ApiError, bounded_error_message, json_error, json_status_error, skin_auth_store_error,
    skin_cape_error, skin_preserve_download_error, skin_preserve_invalid_error, skin_reset_error,
    skin_upload_error,
};
use super::image::{normalize_skin_png, texture_key};
use super::provider::{
    MinecraftCapeSyncClient, MinecraftSkinResetClient, MinecraftSkinTextureClient,
    MinecraftSkinUploadClient, active_minecraft_cape_id, sane_minecraft_texture_url_with_prefix,
    select_sane_minecraft_skin_with_prefix, skin_variant,
};
use super::saved::{
    PendingSkinApplyChange, PendingSkinApplyFilter, SAVED_SKIN_PROFILE_SOURCE,
    clear_pending_saved_skin_apply_for_active_account, clear_pending_saved_skin_apply_for_login_id,
    clear_saved_skin_applied, default_profile_skin_name, insert_pending_saved_skin_apply,
    list_saved_skins, mark_saved_skin_applied, pending_saved_skin_apply_flush_guard,
    read_saved_skin_png, restore_pending_saved_skin_apply, save_saved_skin,
    take_pending_saved_skin_apply, update_saved_skin_metadata, validate_saved_skin_name,
    validate_texture_key,
};

const SKIN_CHANGE_DEBOUNCE: Duration = Duration::from_secs(10);

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ApplySavedSkinQuery {
    pub(super) defer: Option<bool>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SkinApplyResponse {
    pub(crate) status: &'static str,
    pub(crate) texture_key: String,
    pub(crate) profile_updated: bool,
    pub(crate) view_model: SkinCommandViewModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct SkinProfileResetResponse {
    pub(crate) status: &'static str,
    pub(crate) profile_updated: bool,
    pub(crate) view_model: SkinCommandViewModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct SkinCapeResetResponse {
    pub(crate) status: &'static str,
    pub(crate) profile_updated: bool,
    pub(crate) view_model: SkinCommandViewModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct SkinFlushResponse {
    pub(crate) status: &'static str,
    pub(crate) applied: usize,
    pub(crate) view_model: SkinCommandViewModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct SkinPendingClearResponse {
    pub(crate) status: &'static str,
    pub(crate) cleared: bool,
    pub(crate) view_model: SkinCommandViewModel,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SkinCommandViewModel {
    pub(crate) summary: &'static str,
}

pub(crate) async fn handle_apply_saved_skin(
    state: &AppState,
    texture_key: String,
    query: ApplySavedSkinQuery,
    handoff: RequestProducerHandoff,
) -> Result<Json<SkinApplyResponse>, ApiError> {
    if query.defer.unwrap_or(false) {
        return queue_saved_skin_apply(state, texture_key, handoff).await;
    }

    handle_apply_saved_skin_with_client(
        state,
        texture_key,
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub(crate) async fn handle_skin_profile_reset(
    state: &AppState,
) -> Result<Json<SkinProfileResetResponse>, ApiError> {
    handle_skin_profile_reset_with_clients(
        state,
        MinecraftSkinResetClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub(super) async fn handle_skin_profile_reset_with_clients(
    state: &AppState,
    reset_client: MinecraftSkinResetClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Json<SkinProfileResetResponse>, ApiError> {
    let account = active_ready_minecraft_account_for_skin_change(
        state,
        "Minecraft account is not ready for skin reset",
    )
    .await?;
    let saved_skins = list_saved_skins(state).await?;
    preserve_current_profile_skin_before_change(
        state,
        &account,
        &saved_skins,
        &texture_client,
        None,
    )
    .await?;

    let reset_profile = reset_client
        .reset(&account.access_token)
        .await
        .map_err(skin_reset_error)?;
    clear_pending_saved_skin_apply_for_login_id(&account.login_id).await;
    clear_saved_skin_applied(state).await?;
    let profile_updated = if let Some(profile) = reset_profile {
        state
            .auth_logins()
            .update_active_current_minecraft_profile(&account.login_id, profile)
            .await
            .map_err(|_| skin_auth_store_error())?
    } else {
        false
    };

    Ok(Json(SkinProfileResetResponse {
        status: "reset",
        profile_updated,
        view_model: SkinCommandViewModel {
            summary: "Profile skin reset to default.",
        },
    }))
}

pub(crate) async fn handle_skin_cape_reset(
    state: &AppState,
) -> Result<Json<SkinCapeResetResponse>, ApiError> {
    handle_skin_cape_reset_with_clients(
        state,
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub(super) async fn handle_skin_cape_reset_with_clients(
    state: &AppState,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Json<SkinCapeResetResponse>, ApiError> {
    let account = active_ready_minecraft_account_for_skin_change(
        state,
        "Minecraft account is not ready for cape reset",
    )
    .await?;
    let saved_skins = list_saved_skins(state).await?;
    preserve_current_profile_skin_before_change(
        state,
        &account,
        &saved_skins,
        &texture_client,
        None,
    )
    .await?;

    let cape_profile = cape_client
        .sync(&account.access_token, &account.profile, None)
        .await
        .map_err(skin_cape_error)?;
    clear_pending_saved_skin_apply_for_login_id(&account.login_id).await;
    clear_saved_skin_applied(state).await?;
    let profile_updated = if let Some(profile) = cape_profile {
        state
            .auth_logins()
            .update_active_current_minecraft_profile(&account.login_id, profile)
            .await
            .map_err(|_| skin_auth_store_error())?
    } else {
        false
    };

    Ok(Json(SkinCapeResetResponse {
        status: "reset",
        profile_updated,
        view_model: SkinCommandViewModel {
            summary: "Profile cape reset.",
        },
    }))
}

pub(crate) async fn handle_flush_saved_skin_applies(
    state: &AppState,
) -> Result<Json<SkinFlushResponse>, ApiError> {
    let applied = flush_pending_saved_skin_applies_for_active_account_with_clients(
        state,
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await?;

    Ok(Json(SkinFlushResponse {
        status: "flushed",
        applied,
        view_model: SkinCommandViewModel {
            summary: if applied > 0 {
                "Skin applied."
            } else {
                "No skin change was pending."
            },
        },
    }))
}

pub(crate) async fn handle_clear_pending_saved_skin_apply(
    state: &AppState,
) -> Result<Json<SkinPendingClearResponse>, ApiError> {
    let cleared = clear_pending_saved_skin_apply_for_active_account(state).await;

    Ok(Json(SkinPendingClearResponse {
        status: "cleared",
        cleared,
        view_model: SkinCommandViewModel {
            summary: if cleared {
                "Skin change canceled."
            } else {
                "No skin change was pending."
            },
        },
    }))
}

pub(super) async fn handle_apply_saved_skin_with_client(
    state: &AppState,
    texture_key: String,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Json<SkinApplyResponse>, ApiError> {
    apply_saved_skin_now_with_clients(state, texture_key, skin_client, cape_client, texture_client)
        .await
        .map(Json)
}

async fn apply_saved_skin_now_with_clients(
    state: &AppState,
    texture_key: String,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<SkinApplyResponse, ApiError> {
    // Direct apply uploads immediately; login/shutdown paths use the pending queue.
    let texture_key = validate_texture_key(&texture_key)?;
    let account = active_ready_minecraft_account_for_skin_apply(state).await?;
    apply_saved_skin_for_account_with_clients(
        state,
        account,
        texture_key,
        skin_client,
        cape_client,
        texture_client,
    )
    .await
}

async fn apply_saved_skin_for_login_with_clients(
    state: &AppState,
    login_id: &str,
    texture_key: String,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<SkinApplyResponse, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let account = ready_minecraft_account_for_login_id_for_skin_apply(state, login_id).await?;
    apply_saved_skin_for_account_with_clients(
        state,
        account,
        texture_key,
        skin_client,
        cape_client,
        texture_client,
    )
    .await
}

async fn apply_saved_skin_for_account_with_clients(
    state: &AppState,
    account: AuthLoginMinecraftAccount,
    texture_key: String,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<SkinApplyResponse, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let saved_skins = list_saved_skins(state).await?;
    let saved_skin = saved_skins
        .iter()
        .find(|skin| skin.texture_key == texture_key)
        .cloned()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;
    let Some(png_bytes) = read_saved_skin_png(state, texture_key.clone()).await? else {
        return Err(json_error(StatusCode::NOT_FOUND, "saved skin not found"));
    };

    preserve_current_profile_skin_before_apply(
        state,
        &account,
        &saved_skin,
        &saved_skins,
        &texture_client,
    )
    .await?;

    let uploaded_profile = skin_client
        .upload(&account.access_token, &saved_skin.variant, png_bytes)
        .await
        .map_err(skin_upload_error)?;
    let profile_after_upload = uploaded_profile
        .as_ref()
        .filter(|profile| !profile.capes.is_empty())
        .unwrap_or(&account.profile);
    let cape_profile = cape_client
        .sync(
            &account.access_token,
            profile_after_upload,
            saved_skin.cape_id.as_deref(),
        )
        .await
        .map_err(skin_cape_error)?;
    mark_saved_skin_applied(state, texture_key.clone())
        .await?
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;
    let profile_updated = if let Some(profile) = cape_profile.or(uploaded_profile) {
        state
            .auth_logins()
            .update_active_current_minecraft_profile(&account.login_id, profile)
            .await
            .map_err(|_| skin_auth_store_error())?
    } else {
        false
    };

    Ok(SkinApplyResponse {
        status: "applied",
        texture_key,
        profile_updated,
        view_model: SkinCommandViewModel {
            summary: "Skin applied.",
        },
    })
}

async fn queue_saved_skin_apply(
    state: &AppState,
    texture_key: String,
    handoff: RequestProducerHandoff,
) -> Result<Json<SkinApplyResponse>, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let account = active_ready_minecraft_account_for_skin_apply(state).await?;
    let saved_skins = list_saved_skins(state).await?;
    if !saved_skins
        .iter()
        .any(|skin| skin.texture_key == texture_key)
    {
        return Err(json_error(StatusCode::NOT_FOUND, "saved skin not found"));
    }

    let producer = handoff.try_claim().map_err(|_| {
        json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Skin changes are unavailable while the application is shutting down",
        )
    })?;
    set_pending_saved_skin_apply(
        state.clone(),
        PendingSkinApplyChange {
            login_id: account.login_id,
            texture_key: texture_key.clone(),
        },
        producer,
    )
    .await;

    Ok(Json(SkinApplyResponse {
        status: "queued",
        texture_key,
        profile_updated: false,
        view_model: SkinCommandViewModel {
            summary: "Skin will apply shortly.",
        },
    }))
}

async fn active_ready_minecraft_account_for_skin_apply(
    state: &AppState,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    active_ready_minecraft_account_for_skin_change(
        state,
        "Minecraft account is not ready for skin upload",
    )
    .await
}

async fn active_ready_minecraft_account_for_skin_change(
    state: &AppState,
    not_ready_message: &'static str,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    let minecraft_state = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::UNAUTHORIZED,
                "Minecraft account login required",
                "minecraft_account_required",
            )
        })?;
    let account = minecraft_state.account;
    if !account.owns_minecraft_java
        || account.profile.id.trim().is_empty()
        || account.profile.name.trim().is_empty()
    {
        return Err(json_status_error(
            StatusCode::CONFLICT,
            not_ready_message,
            "minecraft_account_not_ready",
        ));
    }

    Ok(account)
}

async fn ready_minecraft_account_for_login_id_for_skin_apply(
    state: &AppState,
    login_id: &str,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    ready_minecraft_account_for_login_id_for_skin_change(
        state,
        login_id,
        "Minecraft account is not ready for skin upload",
    )
    .await
}

async fn ready_minecraft_account_for_login_id_for_skin_change(
    state: &AppState,
    login_id: &str,
    not_ready_message: &'static str,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    let minecraft_state = state
        .auth_logins()
        .current_minecraft_account_state_for_login(login_id)
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::UNAUTHORIZED,
                "Minecraft account login required",
                "minecraft_account_required",
            )
        })?;
    let account = minecraft_state.account;
    if !account.owns_minecraft_java
        || account.profile.id.trim().is_empty()
        || account.profile.name.trim().is_empty()
    {
        return Err(json_status_error(
            StatusCode::CONFLICT,
            not_ready_message,
            "minecraft_account_not_ready",
        ));
    }

    Ok(account)
}

pub(crate) async fn flush_pending_saved_skin_applies_for_launch(
    state: &AppState,
) -> Result<usize, (StatusCode, Json<serde_json::Value>)> {
    flush_pending_saved_skin_applies_for_active_account_with_clients(
        state,
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub async fn flush_pending_saved_skin_applies_for_shutdown(
    state: &AppState,
) -> Result<usize, (StatusCode, Json<serde_json::Value>)> {
    flush_pending_saved_skin_applies_for_active_account_with_clients(
        state,
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn flush_pending_saved_skin_applies_for_active_account_with_clients(
    state: &AppState,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<usize, ApiError> {
    let Some(account_state) = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
    else {
        return Ok(0);
    };

    flush_pending_saved_skin_applies_with_clients(
        state,
        PendingSkinApplyFilter::Login(account_state.account.login_id),
        skin_client,
        cape_client,
        texture_client,
    )
    .await
}

async fn set_pending_saved_skin_apply(
    state: AppState,
    change: PendingSkinApplyChange,
    producer: ProducerLease,
) {
    let schedule = insert_pending_saved_skin_apply(change).await;
    schedule_pending_saved_skin_apply_flush(
        state,
        schedule.login_id,
        schedule.generation,
        producer,
    );
}

fn schedule_pending_saved_skin_apply_flush(
    state: AppState,
    login_id: String,
    generation: u64,
    producer: ProducerLease,
) {
    let retry_owner = producer.claim_child();
    producer.spawn(async move {
        let mut shutdown = state.subscribe_shutdown();
        let allow_retry = if *shutdown.borrow_and_update() {
            false
        } else {
            tokio::select! {
                _ = tokio::time::sleep(SKIN_CHANGE_DEBOUNCE) => true,
                changed = shutdown.changed() => changed.is_err() || !*shutdown.borrow_and_update(),
            }
        };
        let filter = PendingSkinApplyFilter::Generation {
            login_id,
            generation,
        };
        let result = if allow_retry {
            flush_pending_saved_skin_applies_with_clients_owned(
                &state,
                filter,
                MinecraftSkinUploadClient::default(),
                MinecraftCapeSyncClient::default(),
                MinecraftSkinTextureClient::default(),
                retry_owner,
            )
            .await
        } else {
            drop(retry_owner);
            flush_pending_saved_skin_applies_with_clients(
                &state,
                filter,
                MinecraftSkinUploadClient::default(),
                MinecraftCapeSyncClient::default(),
                MinecraftSkinTextureClient::default(),
            )
            .await
        };
        if let Err(error) = result {
            tracing::warn!(
                "failed to flush pending Minecraft skin change: {}",
                bounded_error_message(&error)
            );
        }
    });
}

pub(super) async fn flush_pending_saved_skin_applies_with_clients(
    state: &AppState,
    filter: PendingSkinApplyFilter,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<usize, ApiError> {
    flush_pending_saved_skin_applies_with_clients_inner(
        state,
        filter,
        skin_client,
        cape_client,
        texture_client,
        None,
    )
    .await
}

async fn flush_pending_saved_skin_applies_with_clients_owned(
    state: &AppState,
    filter: PendingSkinApplyFilter,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
    retry_owner: ProducerLease,
) -> Result<usize, ApiError> {
    flush_pending_saved_skin_applies_with_clients_inner(
        state,
        filter,
        skin_client,
        cape_client,
        texture_client,
        Some(retry_owner),
    )
    .await
}

async fn flush_pending_saved_skin_applies_with_clients_inner(
    state: &AppState,
    filter: PendingSkinApplyFilter,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
    retry_owner: Option<ProducerLease>,
) -> Result<usize, ApiError> {
    let _guard = pending_saved_skin_apply_flush_guard().await;
    let Some(entry) = take_pending_saved_skin_apply(&filter).await else {
        return Ok(0);
    };

    let result = apply_saved_skin_for_login_with_clients(
        state,
        &entry.change.login_id,
        entry.change.texture_key.clone(),
        skin_client.clone(),
        cape_client.clone(),
        texture_client.clone(),
    )
    .await;

    match result {
        Ok(_) => Ok(1),
        Err(error) => {
            let schedule = restore_pending_saved_skin_apply(entry).await;
            if let Some(retry_owner) = retry_owner {
                schedule_pending_saved_skin_apply_flush(
                    state.clone(),
                    schedule.login_id,
                    schedule.generation,
                    retry_owner,
                );
            }
            Err(error)
        }
    }
}

async fn preserve_current_profile_skin_before_apply(
    state: &AppState,
    account: &AuthLoginMinecraftAccount,
    target_skin: &SavedSkinRecord,
    saved_skins: &[SavedSkinRecord],
    client: &MinecraftSkinTextureClient,
) -> Result<(), ApiError> {
    preserve_current_profile_skin_before_change(
        state,
        account,
        saved_skins,
        client,
        Some(target_skin.texture_key.as_str()),
    )
    .await
}

async fn preserve_current_profile_skin_before_change(
    state: &AppState,
    account: &AuthLoginMinecraftAccount,
    saved_skins: &[SavedSkinRecord],
    client: &MinecraftSkinTextureClient,
    skip_texture_key: Option<&str>,
) -> Result<(), ApiError> {
    // Do not overwrite a remote profile skin until the current external skin is saved locally.
    let Some(profile_skin) =
        select_sane_minecraft_skin_with_prefix(&account.profile.skins, client.allowed_prefix())
    else {
        return Ok(());
    };
    let Some(texture_url) =
        sane_minecraft_texture_url_with_prefix(&profile_skin.url, client.allowed_prefix())
    else {
        return Ok(());
    };

    let cache_path = profile_skin_file_cache_path(&state.config().paths().config_dir, &texture_url);
    let bytes = match read_profile_skin_file_cache(&cache_path).await {
        Some(bytes) => bytes,
        None => client
            .download(&texture_url)
            .await
            .map_err(skin_preserve_download_error)?,
    };
    let normalized = normalize_skin_png(&bytes).map_err(|_| skin_preserve_invalid_error())?;
    let _ = write_profile_file_cache(&cache_path, &normalized.png_bytes).await;
    let current_texture_key = texture_key(&normalized.png_bytes);
    if skip_texture_key == Some(current_texture_key.as_str()) {
        return Ok(());
    }

    let current_variant = skin_variant(&profile_skin.variant).to_string();
    let current_cape_id = active_minecraft_cape_id(&account.profile);
    if let Some(saved_skin) = saved_skins
        .iter()
        .find(|skin| skin.texture_key == current_texture_key)
    {
        if saved_skin.variant != current_variant || saved_skin.cape_id != current_cape_id {
            let _ = update_saved_skin_metadata(
                state,
                current_texture_key,
                None,
                Some(current_variant),
                Some(current_cape_id),
            )
            .await?;
        }
        return Ok(());
    }

    let name = validate_saved_skin_name(&default_profile_skin_name(&account.profile.name))?;
    let _ = save_saved_skin(
        state,
        current_texture_key,
        name,
        current_variant,
        SAVED_SKIN_PROFILE_SOURCE.to_string(),
        current_cape_id,
        normalized.png_bytes,
    )
    .await?;

    Ok(())
}
