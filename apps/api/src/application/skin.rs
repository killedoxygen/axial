mod cache;
mod errors;
mod image;
mod library;
mod profile_change;
mod profile_media;
mod provider;
mod saved;

pub(crate) use saved::{
    clear_all_pending_saved_skin_applies, clear_pending_saved_skin_apply_for_login_id,
};

#[cfg(test)]
pub(crate) use saved::test_set_pending_saved_skin_apply_for_login_id;

#[cfg(test)]
use crate::state::skins::SavedSkinRecord;
#[cfg(test)]
use crate::state::{AppState, AuthLoginMinecraftAccount};
#[cfg(test)]
use axial_minecraft::offline_uuid;
#[cfg(test)]
use axum::{
    Json,
    body::Body,
    http::{Response, StatusCode, header},
};
#[cfg(test)]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
#[cfg(test)]
use cache::{
    PROFILE_CAPE_FILE_CACHE_CONTROL, PROFILE_SKIN_FILE_CACHE_CONTROL, profile_cape_file_cache_path,
    profile_skin_file_cache_path,
};
#[cfg(test)]
use image::{
    LEGACY_SKIN_HEIGHT, PNG_SIGNATURE, SKIN_HEIGHT, SKIN_WIDTH, decode_skin_png,
    is_valid_normalized_skin_cache_png, normalize_legacy_skin_rgba, normalize_skin_png,
    texture_key,
};
#[cfg(test)]
use library::{
    SAVED_SKIN_FILE_CACHE_CONTROL, handle_save_skin_from_profile_with_client,
    handle_save_skin_from_username_with_clients,
};
#[cfg(test)]
use profile_change::{
    flush_pending_saved_skin_applies_with_clients, handle_apply_saved_skin_with_client,
    handle_skin_cape_reset_with_clients, handle_skin_profile_reset_with_clients,
};
#[cfg(test)]
use profile_media::{
    DEFAULT_HEAD_SIZE, HEAD_CACHE_CONTROL, handle_skin_cape_file_with_client,
    handle_skin_lookup_cape_with_clients, handle_skin_lookup_file_with_clients,
    handle_skin_lookup_head_with_clients, handle_skin_lookup_with_client,
    handle_skin_profile_file_with_client, offline_head_svg, offline_variant,
};
#[cfg(test)]
use provider::{
    AXIAL_USER_AGENT, MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES, MinecraftCapeSyncClient,
    MinecraftSkinResetClient, MinecraftSkinTextureClient, MinecraftSkinUploadClient,
    MinecraftSkinUsernameClient, sane_minecraft_texture_url,
};
#[cfg(test)]
use saved::{
    PendingSkinApplyFilter, SAVED_SKIN_PROFILE_SOURCE, SAVED_SKIN_SOURCE,
    SAVED_SKIN_USERNAME_SOURCE,
};

pub(crate) use library::{
    ReplaceSavedSkinTextureQuery, SaveSkinQuery, UpdateSavedSkinRequest, handle_delete_skin,
    handle_replace_saved_skin_texture, handle_save_skin, handle_save_skin_from_profile,
    handle_save_skin_from_username, handle_saved_skin_file, handle_saved_skins,
    handle_skin_normalize, handle_update_saved_skin,
};
#[cfg(test)]
pub(crate) use library::{
    SaveSkinFromProfileRequest, SaveSkinFromUsernameRequest, SavedSkinsResponse,
    SkinNormalizeResponse,
};

pub use profile_change::flush_pending_saved_skin_applies_for_shutdown;
pub(crate) use profile_change::{
    ApplySavedSkinQuery, flush_pending_saved_skin_applies_for_launch, handle_apply_saved_skin,
    handle_clear_pending_saved_skin_apply, handle_flush_saved_skin_applies, handle_skin_cape_reset,
    handle_skin_profile_reset,
};
pub(crate) use profile_media::{
    SkinCapeFileQuery, SkinLookupQuery, SkinProfileFileQuery, SkinQuery, handle_skin_cape_file,
    handle_skin_head, handle_skin_lookup, handle_skin_lookup_cape, handle_skin_lookup_file,
    handle_skin_lookup_head, handle_skin_profile, handle_skin_profile_file,
};

#[cfg(test)]
pub(crate) use profile_change::{
    SkinApplyResponse, SkinCapeResetResponse, SkinCommandViewModel, SkinFlushResponse,
    SkinPendingClearResponse, SkinProfileResetResponse,
};
#[cfg(test)]
pub(crate) use profile_media::{SkinLookupResponse, SkinProfileResponse};

const SKIN_UPLOAD_MAX_BYTES: usize = 256 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use crate::state::{
        AuthLoginMinecraftCape, AuthLoginMinecraftProfile, AuthLoginMinecraftSkin,
        NewAuthLoginMinecraftAccount, NewAuthLoginMsaToken,
    };
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use axum::{
        body::{Bytes, to_bytes},
        extract::{Path, State as AxumState},
        http::HeaderMap,
        routing::{delete, get, post},
    };
    use std::{fs, io::Cursor, path::PathBuf, sync::Arc};
    use tokio::sync::mpsc;

    mod profile_change;
    mod profile_media;
    mod saved_library;

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str, username: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(
                ConfigStore::from_config(
                    paths.clone(),
                    AppConfig {
                        username: username.to_string(),
                        ..AppConfig::default()
                    },
                )
                .expect("set username"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("load instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(&paths.config_dir)
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
            });

            Self { state, root }
        }

        async fn profile(
            &self,
            username: Option<String>,
            size: Option<u32>,
        ) -> Result<Json<SkinProfileResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_profile(&self.state, SkinQuery { username, size }).await
        }

        async fn head(
            &self,
            username: Option<String>,
            size: Option<u32>,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_head(&self.state, SkinQuery { username, size }).await
        }

        async fn lookup(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Json<SkinLookupResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_with_client(
                SkinLookupQuery {
                    username: username.to_string(),
                    size,
                },
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                allowed_prefix,
            )
            .await
        }

        async fn lookup_head(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_head_with_clients(
                &self.state,
                SkinLookupQuery {
                    username: username.to_string(),
                    size,
                },
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn lookup_file(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_file_with_clients(
                &self.state,
                SkinLookupQuery {
                    username: username.to_string(),
                    size,
                },
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn lookup_cape(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_cape_with_clients(
                &self.state,
                SkinLookupQuery {
                    username: username.to_string(),
                    size,
                },
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn profile_file(
            &self,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            self.profile_file_with_texture(allowed_prefix, None).await
        }

        async fn profile_file_with_texture(
            &self,
            allowed_prefix: String,
            texture: Option<String>,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_profile_file_with_client(
                &self.state,
                SkinProfileFileQuery { texture },
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn cape_file(
            &self,
            cape_id: &str,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_cape_file_with_client(
                &self.state,
                SkinCapeFileQuery {
                    id: cape_id.to_string(),
                },
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn saved_skins(
            &self,
        ) -> Result<Json<SavedSkinsResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_saved_skins(&self.state).await
        }

        async fn save_skin(
            &self,
            name: &str,
            variant: Option<String>,
            body: Vec<u8>,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_save_skin(
                &self.state,
                SaveSkinQuery {
                    name: Some(name.to_string()),
                    variant,
                    cape_id: None,
                    source: None,
                },
                Body::from(body),
            )
            .await
        }

        async fn delete_saved_skin(
            &self,
            texture_key: &str,
        ) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
            handle_delete_skin(&self.state, texture_key.to_string()).await
        }

        async fn update_saved_skin(
            &self,
            texture_key: &str,
            payload: serde_json::Value,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            let payload = serde_json::from_value::<UpdateSavedSkinRequest>(payload)
                .expect("valid update payload");
            handle_update_saved_skin(&self.state, texture_key.to_string(), payload).await
        }

        async fn replace_saved_skin_texture(
            &self,
            texture_key: &str,
            query: ReplaceSavedSkinTextureQuery,
            body: Vec<u8>,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_replace_saved_skin_texture(
                &self.state,
                texture_key.to_string(),
                query,
                Body::from(body),
            )
            .await
        }

        async fn saved_skin_file(
            &self,
            texture_key: &str,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_saved_skin_file(&self.state, texture_key.to_string()).await
        }

        async fn save_skin_from_profile(
            &self,
            payload: SaveSkinFromProfileRequest,
            allowed_prefix: String,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_save_skin_from_profile_with_client(
                &self.state,
                payload,
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn save_skin_from_username(
            &self,
            payload: SaveSkinFromUsernameRequest,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_save_skin_from_username_with_clients(
                &self.state,
                payload,
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn apply_saved_skin_with_endpoint(
            &self,
            texture_key: &str,
            endpoint: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            self.apply_saved_skin_with_endpoints(texture_key, endpoint, "http://127.0.0.1:9/capes")
                .await
        }

        async fn apply_saved_skin_with_endpoints(
            &self,
            texture_key: &str,
            skin_endpoint: &str,
            cape_endpoint: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            self.apply_saved_skin_with_all_endpoints(
                texture_key,
                skin_endpoint,
                cape_endpoint,
                "http://127.0.0.1:9/texture/",
            )
            .await
        }

        async fn apply_saved_skin_with_all_endpoints(
            &self,
            texture_key: &str,
            skin_endpoint: &str,
            cape_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_apply_saved_skin_with_client(
                &self.state,
                texture_key.to_string(),
                MinecraftSkinUploadClient::with_endpoint(skin_endpoint.to_string()),
                MinecraftCapeSyncClient::with_endpoint(cape_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
            )
            .await
        }

        async fn reset_profile_skin_with_endpoints(
            &self,
            reset_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinProfileResetResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_profile_reset_with_clients(
                &self.state,
                MinecraftSkinResetClient::with_endpoint(reset_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
            )
            .await
        }

        async fn reset_profile_cape_with_endpoints(
            &self,
            cape_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinCapeResetResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_cape_reset_with_clients(
                &self.state,
                MinecraftCapeSyncClient::with_endpoint(cape_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
            )
            .await
        }

        async fn queue_saved_skin_apply(
            &self,
            texture_key: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            let request = self.state.try_admit_request().expect("admit skin request");
            handle_apply_saved_skin(
                &self.state,
                texture_key.to_string(),
                ApplySavedSkinQuery { defer: Some(true) },
                request.producer_handoff(),
            )
            .await
        }

        async fn clear_pending_saved_skin_apply(
            &self,
        ) -> Result<Json<SkinPendingClearResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_clear_pending_saved_skin_apply(&self.state).await
        }

        async fn flush_saved_skin_applies_with_endpoints(
            &self,
            skin_endpoint: &str,
            cape_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinFlushResponse>, (StatusCode, Json<serde_json::Value>)> {
            let login_id = self
                .state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await
                .expect("active minecraft account")
                .account
                .login_id;
            let applied = flush_pending_saved_skin_applies_with_clients(
                &self.state,
                PendingSkinApplyFilter::Login(login_id),
                MinecraftSkinUploadClient::with_endpoint(skin_endpoint.to_string()),
                MinecraftCapeSyncClient::with_endpoint(cape_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
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

        async fn add_minecraft_account(&self, profile: AuthLoginMinecraftProfile) {
            self.add_minecraft_account_with_expiry(profile, 86_400)
                .await;
        }

        async fn add_minecraft_account_with_ownership(
            &self,
            profile: AuthLoginMinecraftProfile,
            owns_minecraft_java: bool,
        ) {
            self.add_minecraft_account_with_expiry_and_ownership(
                profile,
                86_400,
                owns_minecraft_java,
            )
            .await;
        }

        async fn add_minecraft_account_with_expiry(
            &self,
            profile: AuthLoginMinecraftProfile,
            expires_in: u64,
        ) {
            self.add_minecraft_account_with_expiry_and_ownership(profile, expires_in, true)
                .await;
        }

        async fn add_minecraft_account_with_expiry_and_ownership(
            &self,
            profile: AuthLoginMinecraftProfile,
            expires_in: u64,
            owns_minecraft_java: bool,
        ) -> AuthLoginMinecraftAccount {
            self.add_minecraft_account_with_tokens_and_expiry_and_ownership(
                profile,
                "msa-access-token",
                "minecraft-access-token",
                expires_in,
                owns_minecraft_java,
            )
            .await
        }

        async fn add_minecraft_account_with_tokens(
            &self,
            profile: AuthLoginMinecraftProfile,
            msa_access_token: &str,
            minecraft_access_token: &str,
        ) -> AuthLoginMinecraftAccount {
            self.add_minecraft_account_with_tokens_and_expiry_and_ownership(
                profile,
                msa_access_token,
                minecraft_access_token,
                86_400,
                true,
            )
            .await
        }

        async fn add_minecraft_account_with_tokens_and_expiry_and_ownership(
            &self,
            profile: AuthLoginMinecraftProfile,
            msa_access_token: &str,
            minecraft_access_token: &str,
            expires_in: u64,
            owns_minecraft_java: bool,
        ) -> AuthLoginMinecraftAccount {
            let (_token, account) = self
                .state
                .auth_logins()
                .replace_with_msa_and_minecraft_account(
                    NewAuthLoginMsaToken {
                        access_token: msa_access_token.to_string(),
                        refresh_token: Some("msa-refresh-token".to_string()),
                        id_token: None,
                        token_type: "Bearer".to_string(),
                        expires_in: 3600,
                        scope: Some("XboxLive.signin offline_access".to_string()),
                    },
                    NewAuthLoginMinecraftAccount {
                        access_token: minecraft_access_token.to_string(),
                        token_type: Some("Bearer".to_string()),
                        expires_in,
                        profile,
                        owns_minecraft_java,
                    },
                )
                .await
                .expect("insert Minecraft account fixture");
            account
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-api-skin-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
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

    async fn response_body(response: Response<Body>) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    async fn response_bytes(response: Response<Body>) -> Vec<u8> {
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body")
            .to_vec()
    }

    async fn normalize_skin_body(
        body: Vec<u8>,
    ) -> Result<Json<SkinNormalizeResponse>, (StatusCode, Json<serde_json::Value>)> {
        handle_skin_normalize(Body::from(body)).await
    }

    async fn skin_apply_route_test_server(
        mode: SkinApplyServerMode,
    ) -> (String, mpsc::UnboundedReceiver<RecordedSkinApplyRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/minecraft/profile/skins", post(record_skin_apply_route))
            .with_state(SkinApplyRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind skin apply route test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("skin apply route test server");
        });

        (format!("{base_url}/minecraft/profile/skins"), rx)
    }

    async fn skin_reset_route_test_server(
        mode: SkinResetServerMode,
    ) -> (String, mpsc::UnboundedReceiver<RecordedSkinResetRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/minecraft/profile/skins/active",
                delete(record_skin_reset_route),
            )
            .with_state(SkinResetRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind skin reset route test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("skin reset route test server");
        });

        (format!("{base_url}/minecraft/profile/skins/active"), rx)
    }

    async fn cape_sync_route_test_server(
        mode: CapeSyncServerMode,
    ) -> (String, mpsc::UnboundedReceiver<RecordedCapeSyncRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/minecraft/profile/capes/active",
                axum::routing::put(record_cape_sync_route).delete(record_cape_sync_route),
            )
            .with_state(CapeSyncRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind cape sync route test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("cape sync route test server");
        });

        (format!("{base_url}/minecraft/profile/capes/active"), rx)
    }

    async fn skin_profile_texture_test_server(
        mode: SkinProfileTextureServerMode,
    ) -> (
        String,
        mpsc::UnboundedReceiver<RecordedSkinProfileTextureRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/texture/{texture_id}",
                get(record_skin_profile_texture_route),
            )
            .with_state(SkinProfileTextureRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind skin profile texture test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("skin profile texture test server");
        });

        (format!("{base_url}/texture/"), rx)
    }

    async fn minecraft_username_test_server(
        mode: MinecraftUsernameServerMode,
    ) -> (
        String,
        String,
        mpsc::UnboundedReceiver<RecordedMinecraftUsernameRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/users/profiles/minecraft/{username}",
                get(record_minecraft_username_profile_route),
            )
            .route(
                "/session/minecraft/profile/{uuid}",
                get(record_minecraft_username_session_route),
            )
            .with_state(MinecraftUsernameRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind minecraft username test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("minecraft username test server");
        });

        (
            format!("{base_url}/users/profiles/minecraft"),
            format!("{base_url}/session/minecraft/profile"),
            rx,
        )
    }

    #[derive(Clone, Copy)]
    enum SkinApplyServerMode {
        Success,
        SuccessWithCapeAvailable,
        OversizedSuccess,
        RateLimited,
        Rejected,
    }

    #[derive(Clone, Copy)]
    enum SkinResetServerMode {
        Success,
        RateLimited,
    }

    #[derive(Clone, Copy)]
    enum CapeSyncServerMode {
        Success,
        RateLimited,
    }

    #[derive(Clone)]
    enum SkinProfileTextureServerMode {
        Png(Vec<u8>),
        Oversized,
    }

    #[derive(Clone)]
    enum MinecraftUsernameServerMode {
        Success {
            texture_url: String,
            model: Option<String>,
            cape_url: Option<String>,
        },
        NotFound,
        MissingSkin,
        MalformedTextures,
    }

    #[derive(Clone)]
    struct SkinApplyRouteState {
        tx: mpsc::UnboundedSender<RecordedSkinApplyRequest>,
        mode: SkinApplyServerMode,
    }

    #[derive(Clone)]
    struct SkinResetRouteState {
        tx: mpsc::UnboundedSender<RecordedSkinResetRequest>,
        mode: SkinResetServerMode,
    }

    #[derive(Clone)]
    struct CapeSyncRouteState {
        tx: mpsc::UnboundedSender<RecordedCapeSyncRequest>,
        mode: CapeSyncServerMode,
    }

    #[derive(Clone)]
    struct SkinProfileTextureRouteState {
        tx: mpsc::UnboundedSender<RecordedSkinProfileTextureRequest>,
        mode: SkinProfileTextureServerMode,
    }

    #[derive(Clone)]
    struct MinecraftUsernameRouteState {
        tx: mpsc::UnboundedSender<RecordedMinecraftUsernameRequest>,
        mode: MinecraftUsernameServerMode,
    }

    #[derive(Debug)]
    struct RecordedSkinApplyRequest {
        path: String,
        authorization: Option<String>,
        accept: Option<String>,
        user_agent: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    #[derive(Debug)]
    struct RecordedSkinResetRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        accept: Option<String>,
        user_agent: Option<String>,
    }

    #[derive(Debug)]
    struct RecordedCapeSyncRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        accept: Option<String>,
        user_agent: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    #[derive(Debug)]
    struct RecordedSkinProfileTextureRequest {
        path: String,
        accept: Option<String>,
        user_agent: Option<String>,
    }

    #[derive(Debug)]
    struct RecordedMinecraftUsernameRequest {
        path: String,
        accept: Option<String>,
        user_agent: Option<String>,
    }

    async fn record_skin_apply_route(
        AxumState(state): AxumState<SkinApplyRouteState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedSkinApplyRequest {
            path: "/minecraft/profile/skins".to_string(),
            authorization: header_value(&headers, "authorization"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
            content_type: header_value(&headers, "content-type"),
            body: body.to_vec(),
        });

        match state.mode {
            SkinApplyServerMode::Success => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "updated-profile-id",
                    "name": "UpdatedProfileName",
                    "skins": [{
                        "id": "updated-skin-id",
                        "state": "ACTIVE",
                        "url": "https://textures.minecraft.net/texture/updatedSkin",
                        "variant": "SLIM"
                    }],
                    "capes": []
                })),
            ),
            SkinApplyServerMode::SuccessWithCapeAvailable => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "updated-profile-id",
                    "name": "UpdatedProfileName",
                    "skins": [{
                        "id": "updated-skin-id",
                        "state": "ACTIVE",
                        "url": "https://textures.minecraft.net/texture/updatedSkin",
                        "variant": "SLIM"
                    }],
                    "capes": [{
                        "id": "cape-id",
                        "state": "INACTIVE",
                        "url": "https://textures.minecraft.net/texture/capeTexture"
                    }]
                })),
            ),
            SkinApplyServerMode::OversizedSuccess => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "payload": "x".repeat(MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES + 1),
                })),
            ),
            SkinApplyServerMode::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                })),
            ),
            SkinApplyServerMode::Rejected => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "path": "/home/zero/skin.png",
                    "error": "provider-secret-payload",
                })),
            ),
        }
    }

    async fn record_skin_reset_route(
        AxumState(state): AxumState<SkinResetRouteState>,
        method: axum::http::Method,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedSkinResetRequest {
            method: method.to_string(),
            path: "/minecraft/profile/skins/active".to_string(),
            authorization: header_value(&headers, "authorization"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        match state.mode {
            SkinResetServerMode::Success => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "reset-profile-id",
                    "name": "ResetProfileName",
                    "skins": [],
                    "capes": []
                })),
            ),
            SkinResetServerMode::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                })),
            ),
        }
    }

    async fn record_cape_sync_route(
        AxumState(state): AxumState<CapeSyncRouteState>,
        method: axum::http::Method,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedCapeSyncRequest {
            method: method.to_string(),
            path: "/minecraft/profile/capes/active".to_string(),
            authorization: header_value(&headers, "authorization"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
            content_type: header_value(&headers, "content-type"),
            body: body.to_vec(),
        });

        match state.mode {
            CapeSyncServerMode::Success => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "updated-profile-id",
                    "name": "UpdatedProfileName",
                    "skins": [{
                        "id": "updated-skin-id",
                        "state": "ACTIVE",
                        "url": "https://textures.minecraft.net/texture/updatedSkin",
                        "variant": "SLIM"
                    }],
                    "capes": [{
                        "id": "cape-id",
                        "state": if method == axum::http::Method::PUT { "ACTIVE" } else { "INACTIVE" },
                        "url": "https://textures.minecraft.net/texture/capeTexture"
                    }]
                })),
            ),
            CapeSyncServerMode::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                })),
            ),
        }
    }

    async fn record_skin_profile_texture_route(
        AxumState(state): AxumState<SkinProfileTextureRouteState>,
        Path(texture_id): Path<String>,
        headers: HeaderMap,
    ) -> Response<Body> {
        let _ = state.tx.send(RecordedSkinProfileTextureRequest {
            path: format!("/texture/{texture_id}"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        let (status, body) = match state.mode {
            SkinProfileTextureServerMode::Png(bytes) => (StatusCode::OK, bytes),
            SkinProfileTextureServerMode::Oversized => {
                (StatusCode::OK, vec![0; SKIN_UPLOAD_MAX_BYTES + 1])
            }
        };

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "image/png")
            .body(Body::from(body))
            .expect("skin profile texture response")
    }

    async fn record_minecraft_username_profile_route(
        AxumState(state): AxumState<MinecraftUsernameRouteState>,
        Path(username): Path<String>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedMinecraftUsernameRequest {
            path: format!("/users/profiles/minecraft/{username}"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        match state.mode {
            MinecraftUsernameServerMode::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "provider-secret-payload" })),
            ),
            _ => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "0123456789abcdef0123456789abcdef",
                    "name": "ResolvedName",
                })),
            ),
        }
    }

    async fn record_minecraft_username_session_route(
        AxumState(state): AxumState<MinecraftUsernameRouteState>,
        Path(uuid): Path<String>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedMinecraftUsernameRequest {
            path: format!("/session/minecraft/profile/{uuid}"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        let textures_value = match state.mode {
            MinecraftUsernameServerMode::Success {
                texture_url,
                model,
                cape_url,
            } => {
                let mut skin = serde_json::json!({ "url": texture_url });
                if let Some(model) = model {
                    skin["metadata"] = serde_json::json!({ "model": model });
                }
                let mut textures = serde_json::json!({ "SKIN": skin });
                if let Some(cape_url) = cape_url {
                    textures["CAPE"] = serde_json::json!({ "url": cape_url });
                }
                base64_encode_standard(
                    serde_json::json!({ "textures": textures })
                        .to_string()
                        .as_bytes(),
                )
            }
            MinecraftUsernameServerMode::MissingSkin => {
                base64_encode_standard(br#"{"textures":{}}"#)
            }
            MinecraftUsernameServerMode::MalformedTextures => "not-base64!".to_string(),
            MinecraftUsernameServerMode::NotFound => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "provider-secret-payload" })),
                );
            }
        };

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": "0123456789abcdef0123456789abcdef",
                "name": "ResolvedName",
                "properties": [{
                    "name": "textures",
                    "value": textures_value,
                }],
            })),
        )
    }

    fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    }

    fn body_contains(body: &[u8], needle: &[u8]) -> bool {
        body.windows(needle.len()).any(|window| window == needle)
    }

    fn base64_encode_standard(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = *chunk.get(1).unwrap_or(&0);
            let third = *chunk.get(2).unwrap_or(&0);

            encoded.push(ALPHABET[(first >> 2) as usize] as char);
            encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            if chunk.len() > 1 {
                encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
            } else {
                encoded.push('=');
            }
            if chunk.len() > 2 {
                encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }

        encoded
    }

    fn test_skin_png(width: u32, height: u32) -> Vec<u8> {
        let rgba = test_skin_rgba(width, height);
        encode_test_png(width, height, &rgba)
    }

    fn test_skin_png_with_seed(width: u32, height: u32, seed: u8) -> Vec<u8> {
        let mut rgba = test_skin_rgba(width, height);
        for pixel in rgba.chunks_mut(4) {
            pixel[0] = pixel[0].wrapping_add(seed);
            pixel[1] = pixel[1].wrapping_add(seed.wrapping_mul(3));
            pixel[2] = pixel[2].wrapping_add(seed.wrapping_mul(5));
        }
        encode_test_png(width, height, &rgba)
    }

    fn test_slim_skin_png() -> Vec<u8> {
        let mut rgba = test_skin_rgba(SKIN_WIDTH, SKIN_HEIGHT);
        for y in 20..32 {
            for x in 54..56 {
                let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
                rgba[alpha_index] = 0;
            }
        }

        encode_test_png(SKIN_WIDTH, SKIN_HEIGHT, &rgba)
    }

    fn test_skin_rgba(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[
                    x.wrapping_mul(3) as u8,
                    y.wrapping_mul(5) as u8,
                    x.wrapping_add(y) as u8,
                    255,
                ]);
            }
        }
        rgba
    }

    fn skin_rgba_pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let index = ((y * SKIN_WIDTH + x) * 4) as usize;
        [
            rgba[index],
            rgba[index + 1],
            rgba[index + 2],
            rgba[index + 3],
        ]
    }

    fn set_skin_rgba_alpha(rgba: &mut [u8], x: u32, y: u32, alpha: u8) {
        let index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
        rgba[index] = alpha;
    }

    fn fill_skin_rgba_region(
        rgba: &mut [u8],
        start_x: u32,
        start_y: u32,
        width: u32,
        height: u32,
        pixel: [u8; 4],
    ) {
        for y in start_y..start_y + height {
            for x in start_x..start_x + width {
                let index = ((y * SKIN_WIDTH + x) * 4) as usize;
                rgba[index..index + 4].copy_from_slice(&pixel);
            }
        }
    }

    fn encode_test_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write png header");
            writer.write_image_data(rgba).expect("write png pixels");
        }
        bytes
    }

    fn assert_texture_key(value: &str) {
        assert_eq!(value.len(), 64);
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    fn assert_skin_normalize_error(
        error: (StatusCode, Json<serde_json::Value>),
        expected_status: StatusCode,
        expected_message: &'static str,
    ) {
        assert_eq!(error.0, expected_status);
        assert_eq!(error.1.0, serde_json::json!({ "error": expected_message }));
        assert_eq!(error.1.0.as_object().expect("json object").len(), 1);
    }

    fn test_profile(name: &str, skins: Vec<AuthLoginMinecraftSkin>) -> AuthLoginMinecraftProfile {
        test_profile_with_capes(name, skins, Vec::new())
    }

    fn test_profile_with_capes(
        name: &str,
        skins: Vec<AuthLoginMinecraftSkin>,
        capes: Vec<AuthLoginMinecraftCape>,
    ) -> AuthLoginMinecraftProfile {
        AuthLoginMinecraftProfile {
            id: format!("{name}-id"),
            name: name.to_string(),
            skins,
            capes,
        }
    }

    fn minecraft_skin(id: &str, state: &str, url: &str, variant: &str) -> AuthLoginMinecraftSkin {
        AuthLoginMinecraftSkin {
            id: id.to_string(),
            state: state.to_string(),
            url: url.to_string(),
            variant: variant.to_string(),
        }
    }

    fn minecraft_cape(id: &str, state: &str, url: &str) -> AuthLoginMinecraftCape {
        AuthLoginMinecraftCape {
            id: id.to_string(),
            state: state.to_string(),
            url: url.to_string(),
        }
    }
}
