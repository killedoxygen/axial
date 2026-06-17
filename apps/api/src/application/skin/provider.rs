use crate::state::{AuthLoginMinecraftCape, AuthLoginMinecraftProfile, AuthLoginMinecraftSkin};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{LazyLock, OnceLock},
    time::{Duration, Instant},
};

use super::SKIN_UPLOAD_MAX_BYTES;

pub(super) const MINECRAFT_TEXTURE_URL_PREFIX: &str = "https://textures.minecraft.net/texture/";
const MOJANG_PROFILE_RESPONSE_MAX_BYTES: usize = 16 * 1024;
const MINECRAFT_SESSION_PROFILE_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const MINECRAFT_SESSION_TEXTURES_PROPERTY_MAX_BYTES: usize = 16 * 1024;
pub(super) const MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const MINECRAFT_SKIN_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MINECRAFT_SKIN_HTTP_TIMEOUT: Duration = Duration::from_secs(25);
const MINECRAFT_USERNAME_LOOKUP_CACHE_TTL: Duration = Duration::from_secs(300);
const MINECRAFT_USERNAME_LOOKUP_CACHE_MAX_ENTRIES: usize = 256;
const MOJANG_PROFILE_ENDPOINT: &str = "https://api.mojang.com/users/profiles/minecraft";
const MINECRAFT_SESSION_PROFILE_ENDPOINT: &str =
    "https://sessionserver.mojang.com/session/minecraft/profile";
const MINECRAFT_SKIN_UPLOAD_ENDPOINT: &str =
    "https://api.minecraftservices.com/minecraft/profile/skins";
const MINECRAFT_SKIN_RESET_ENDPOINT: &str =
    "https://api.minecraftservices.com/minecraft/profile/skins/active";
const MINECRAFT_CAPE_ENDPOINT: &str =
    "https://api.minecraftservices.com/minecraft/profile/capes/active";
pub(super) const CROOPOR_USER_AGENT: &str = concat!("croopor/", env!("CARGO_PKG_VERSION"));

static MINECRAFT_USERNAME_SKIN_CACHE: LazyLock<
    tokio::sync::Mutex<HashMap<String, MinecraftUsernameSkinCacheEntry>>,
> = LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

#[derive(Clone)]
struct MinecraftUsernameSkinCacheEntry {
    profile: MinecraftUsernameSkinProfile,
    expires_at: Instant,
}
pub(super) fn select_minecraft_skin(
    skins: &[AuthLoginMinecraftSkin],
) -> Option<&AuthLoginMinecraftSkin> {
    select_minecraft_skin_with_prefix(skins, MINECRAFT_TEXTURE_URL_PREFIX)
}

fn select_minecraft_skin_with_prefix<'a>(
    skins: &'a [AuthLoginMinecraftSkin],
    allowed_prefix: &str,
) -> Option<&'a AuthLoginMinecraftSkin> {
    skins
        .iter()
        .find(|skin| skin.state.eq_ignore_ascii_case("ACTIVE"))
        .or_else(|| {
            skins.iter().find(|skin| {
                sane_minecraft_texture_url_with_prefix(&skin.url, allowed_prefix).is_some()
            })
        })
}

pub(super) fn select_sane_minecraft_skin_with_prefix<'a>(
    skins: &'a [AuthLoginMinecraftSkin],
    allowed_prefix: &str,
) -> Option<&'a AuthLoginMinecraftSkin> {
    skins
        .iter()
        .find(|skin| {
            skin.state.eq_ignore_ascii_case("ACTIVE")
                && sane_minecraft_texture_url_with_prefix(&skin.url, allowed_prefix).is_some()
        })
        .or_else(|| {
            skins.iter().find(|skin| {
                sane_minecraft_texture_url_with_prefix(&skin.url, allowed_prefix).is_some()
            })
        })
}

pub(super) fn active_minecraft_cape_id(profile: &AuthLoginMinecraftProfile) -> Option<String> {
    profile
        .capes
        .iter()
        .find(|cape| cape.state.eq_ignore_ascii_case("ACTIVE"))
        .map(|cape| cape.id.clone())
}

pub(super) fn skin_variant(variant: &str) -> &'static str {
    if variant.eq_ignore_ascii_case("slim") {
        "slim"
    } else {
        "classic"
    }
}

pub(super) fn sane_minecraft_texture_url(url: &str) -> Option<String> {
    sane_minecraft_texture_url_with_prefix(url, MINECRAFT_TEXTURE_URL_PREFIX)
}

pub(super) fn sane_minecraft_texture_url_with_prefix(
    url: &str,
    allowed_prefix: &str,
) -> Option<String> {
    let trimmed = url.trim();
    if trimmed != url {
        return None;
    }

    // Mojang session and profile APIs still hand out http:// texture URLs;
    // canonicalize them to the https prefix before validating and downloading.
    let canonical = match (
        allowed_prefix.strip_prefix("https://"),
        trimmed.strip_prefix("http://"),
    ) {
        (Some(_), Some(rest)) => format!("https://{rest}"),
        _ => trimmed.to_string(),
    };
    if !canonical.starts_with(allowed_prefix) {
        return None;
    }

    let texture_id = &canonical[allowed_prefix.len()..];
    if texture_id.is_empty() || texture_id.len() > 128 {
        return None;
    }
    if !texture_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return None;
    }

    Some(canonical)
}

#[derive(Clone)]
pub(super) struct MinecraftSkinUsernameClient {
    http: reqwest::Client,
    profile_endpoint: String,
    session_profile_endpoint: String,
}

impl MinecraftSkinUsernameClient {
    pub(super) fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            profile_endpoint: MOJANG_PROFILE_ENDPOINT.to_string(),
            session_profile_endpoint: MINECRAFT_SESSION_PROFILE_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_endpoints(
        profile_endpoint: String,
        session_profile_endpoint: String,
    ) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            profile_endpoint,
            session_profile_endpoint,
        }
    }

    pub(super) async fn skin_profile(
        &self,
        username: &str,
        allowed_texture_prefix: &str,
    ) -> Result<MinecraftUsernameSkinProfile, MinecraftUsernameSkinError> {
        if let Some(profile) = self
            .cached_skin_profile(username, allowed_texture_prefix)
            .await
        {
            return Ok(profile);
        }

        let mojang_profile = self.mojang_profile(username).await?;
        if !is_mojang_uuid(&mojang_profile.id) || mojang_profile.name.trim().is_empty() {
            return Err(MinecraftUsernameSkinError::Unavailable);
        }

        let session_profile = self.session_profile(&mojang_profile.id).await?;
        let profile = session_profile
            .skin_profile(allowed_texture_prefix)
            .map(|skin| MinecraftUsernameSkinProfile {
                uuid: mojang_profile.id,
                name: mojang_profile.name,
                variant: skin.variant,
                texture_url: skin.texture_url,
                cape_url: skin.cape_url,
            })?;
        self.store_cached_skin_profile(username, allowed_texture_prefix, &profile)
            .await;
        Ok(profile)
    }

    async fn cached_skin_profile(
        &self,
        username: &str,
        allowed_texture_prefix: &str,
    ) -> Option<MinecraftUsernameSkinProfile> {
        let key = self.lookup_cache_key(username, allowed_texture_prefix);
        let now = Instant::now();
        let mut cache = MINECRAFT_USERNAME_SKIN_CACHE.lock().await;
        if let Some(entry) = cache.get(&key)
            && entry.expires_at > now
        {
            return Some(entry.profile.clone());
        }
        cache.remove(&key);
        None
    }

    async fn store_cached_skin_profile(
        &self,
        requested_username: &str,
        allowed_texture_prefix: &str,
        profile: &MinecraftUsernameSkinProfile,
    ) {
        let expires_at = Instant::now() + MINECRAFT_USERNAME_LOOKUP_CACHE_TTL;
        let entry = MinecraftUsernameSkinCacheEntry {
            profile: profile.clone(),
            expires_at,
        };
        let requested_key = self.lookup_cache_key(requested_username, allowed_texture_prefix);
        let resolved_key = self.lookup_cache_key(&profile.name, allowed_texture_prefix);
        let mut cache = MINECRAFT_USERNAME_SKIN_CACHE.lock().await;
        cache.retain(|_, entry| entry.expires_at > Instant::now());
        if cache.len() >= MINECRAFT_USERNAME_LOOKUP_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(requested_key, entry.clone());
        cache.insert(resolved_key, entry);
    }

    fn lookup_cache_key(&self, username: &str, allowed_texture_prefix: &str) -> String {
        format!(
            "{}\n{}\n{}\n{}",
            self.profile_endpoint,
            self.session_profile_endpoint,
            allowed_texture_prefix,
            username.trim().to_ascii_lowercase()
        )
    }

    async fn mojang_profile(
        &self,
        username: &str,
    ) -> Result<MojangUsernameProfile, MinecraftUsernameSkinError> {
        let response = self
            .http
            .get(format!("{}/{username}", self.profile_endpoint))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| MinecraftUsernameSkinError::Unavailable)?;
        read_minecraft_json_response(response, MOJANG_PROFILE_RESPONSE_MAX_BYTES)
            .await
            .and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| MinecraftUsernameSkinError::Unavailable)
            })
    }

    async fn session_profile(
        &self,
        uuid: &str,
    ) -> Result<MinecraftSessionProfile, MinecraftUsernameSkinError> {
        let response = self
            .http
            .get(format!("{}/{uuid}", self.session_profile_endpoint))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| MinecraftUsernameSkinError::Unavailable)?;
        read_minecraft_json_response(response, MINECRAFT_SESSION_PROFILE_RESPONSE_MAX_BYTES)
            .await
            .and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| MinecraftUsernameSkinError::Unavailable)
            })
    }
}

#[derive(Debug, Clone)]
pub(super) struct MinecraftUsernameSkinProfile {
    pub(super) uuid: String,
    pub(super) name: String,
    pub(super) variant: &'static str,
    pub(super) texture_url: String,
    pub(super) cape_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum MinecraftUsernameSkinError {
    NotFound,
    RateLimited,
    Unavailable,
    MissingSkin,
    MalformedTextures,
}

#[derive(Debug, Deserialize)]
struct MojangUsernameProfile {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionProfile {
    #[serde(default)]
    properties: Vec<MinecraftSessionProfileProperty>,
}

impl MinecraftSessionProfile {
    fn skin_profile(
        &self,
        allowed_texture_prefix: &str,
    ) -> Result<MinecraftSessionSkinProfile, MinecraftUsernameSkinError> {
        let property = self
            .properties
            .iter()
            .find(|property| property.name == "textures")
            .ok_or(MinecraftUsernameSkinError::MissingSkin)?;
        if property.value.len() > MINECRAFT_SESSION_TEXTURES_PROPERTY_MAX_BYTES {
            return Err(MinecraftUsernameSkinError::MalformedTextures);
        }

        let bytes = BASE64_STANDARD
            .decode(property.value.as_bytes())
            .map_err(|_| MinecraftUsernameSkinError::MalformedTextures)?;
        if bytes.len() > MINECRAFT_SESSION_TEXTURES_PROPERTY_MAX_BYTES {
            return Err(MinecraftUsernameSkinError::MalformedTextures);
        }
        let textures: MinecraftSessionTextures = serde_json::from_slice(&bytes)
            .map_err(|_| MinecraftUsernameSkinError::MalformedTextures)?;
        let skin = textures
            .textures
            .skin
            .ok_or(MinecraftUsernameSkinError::MissingSkin)?;
        let texture_url = sane_minecraft_texture_url_with_prefix(&skin.url, allowed_texture_prefix)
            .ok_or(MinecraftUsernameSkinError::MissingSkin)?;
        let cape_url = textures.textures.cape.and_then(|cape| {
            sane_minecraft_texture_url_with_prefix(&cape.url, allowed_texture_prefix)
        });
        let variant = skin
            .metadata
            .as_ref()
            .map(|metadata| skin_variant(&metadata.model))
            .unwrap_or("classic");

        Ok(MinecraftSessionSkinProfile {
            texture_url,
            cape_url,
            variant,
        })
    }
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionProfileProperty {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionTextures {
    textures: MinecraftSessionTextureMap,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionTextureMap {
    #[serde(rename = "SKIN")]
    skin: Option<MinecraftSessionSkinTexture>,
    #[serde(rename = "CAPE")]
    cape: Option<MinecraftSessionCapeTexture>,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionSkinTexture {
    url: String,
    metadata: Option<MinecraftSessionSkinMetadata>,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionSkinMetadata {
    model: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionCapeTexture {
    url: String,
}

struct MinecraftSessionSkinProfile {
    texture_url: String,
    cape_url: Option<String>,
    variant: &'static str,
}

async fn read_minecraft_json_response(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, MinecraftUsernameSkinError> {
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::NO_CONTENT {
        return Err(MinecraftUsernameSkinError::NotFound);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(MinecraftUsernameSkinError::RateLimited);
    }
    if status.is_client_error() || status.is_server_error() {
        return Err(MinecraftUsernameSkinError::Unavailable);
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(MinecraftUsernameSkinError::Unavailable);
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MinecraftUsernameSkinError::Unavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(MinecraftUsernameSkinError::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

fn is_mojang_uuid(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone)]
pub(super) struct MinecraftSkinTextureClient {
    http: reqwest::Client,
    allowed_prefix: String,
}

impl MinecraftSkinTextureClient {
    pub(super) fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            allowed_prefix: MINECRAFT_TEXTURE_URL_PREFIX.to_string(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_allowed_prefix(allowed_prefix: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            allowed_prefix,
        }
    }

    pub(super) fn allowed_prefix(&self) -> &str {
        &self.allowed_prefix
    }

    pub(super) async fn download(&self, url: &str) -> Result<Vec<u8>, SkinTextureDownloadError> {
        if sane_minecraft_texture_url_with_prefix(url, &self.allowed_prefix).is_none() {
            return Err(SkinTextureDownloadError::InvalidUrl);
        }

        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "image/png")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| SkinTextureDownloadError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinTextureDownloadError::RateLimited);
        }
        if status.is_client_error() || status.is_server_error() {
            return Err(SkinTextureDownloadError::Unavailable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > SKIN_UPLOAD_MAX_BYTES as u64)
        {
            return Err(SkinTextureDownloadError::TooLarge);
        }

        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SkinTextureDownloadError::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > SKIN_UPLOAD_MAX_BYTES {
                return Err(SkinTextureDownloadError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }

        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SkinTextureDownloadError {
    InvalidUrl,
    RateLimited,
    TooLarge,
    Unavailable,
}

#[derive(Clone)]
pub(super) struct MinecraftSkinUploadClient {
    http: reqwest::Client,
    endpoint: String,
}

impl MinecraftSkinUploadClient {
    pub(super) fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint: MINECRAFT_SKIN_UPLOAD_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(endpoint: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint,
        }
    }

    pub(super) async fn upload(
        &self,
        access_token: &str,
        variant: &str,
        png_bytes: Vec<u8>,
    ) -> Result<Option<AuthLoginMinecraftProfile>, SkinUploadError> {
        let file = reqwest::multipart::Part::bytes(png_bytes)
            .file_name("skin.png")
            .mime_str("image/png")
            .map_err(|_| SkinUploadError::Unavailable)?;
        let form = reqwest::multipart::Form::new()
            .text("variant", skin_variant(variant).to_string())
            .part("file", file);
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .multipart(form)
            .send()
            .await
            .map_err(|_| SkinUploadError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SkinUploadError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinUploadError::RateLimited);
        }
        if status.is_client_error() {
            return Err(SkinUploadError::Rejected);
        }
        if status.is_server_error() {
            return Err(SkinUploadError::Unavailable);
        }

        let bytes = read_skin_upload_response(response).await?;
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(None);
        }

        let profile = serde_json::from_slice::<SkinUploadMinecraftProfile>(&bytes)
            .ok()
            .map(AuthLoginMinecraftProfile::from);
        Ok(profile)
    }
}

#[derive(Clone)]
pub(super) struct MinecraftSkinResetClient {
    http: reqwest::Client,
    endpoint: String,
}

impl MinecraftSkinResetClient {
    pub(super) fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint: MINECRAFT_SKIN_RESET_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(endpoint: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint,
        }
    }

    pub(super) async fn reset(
        &self,
        access_token: &str,
    ) -> Result<Option<AuthLoginMinecraftProfile>, SkinUploadError> {
        let response = self
            .http
            .delete(&self.endpoint)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| SkinUploadError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SkinUploadError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinUploadError::RateLimited);
        }
        if status.is_client_error() {
            return Err(SkinUploadError::Rejected);
        }
        if status.is_server_error() {
            return Err(SkinUploadError::Unavailable);
        }

        let bytes = read_minecraft_profile_response(response).await?;
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(None);
        }

        let profile = serde_json::from_slice::<SkinUploadMinecraftProfile>(&bytes)
            .ok()
            .map(AuthLoginMinecraftProfile::from);
        Ok(profile)
    }
}

#[derive(Clone)]
pub(super) struct MinecraftCapeSyncClient {
    http: reqwest::Client,
    endpoint: String,
}

impl MinecraftCapeSyncClient {
    pub(super) fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint: MINECRAFT_CAPE_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(endpoint: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint,
        }
    }

    pub(super) async fn sync(
        &self,
        access_token: &str,
        profile: &AuthLoginMinecraftProfile,
        target_cape_id: Option<&str>,
    ) -> Result<Option<AuthLoginMinecraftProfile>, SkinCapeError> {
        if let Some(cape_id) = target_cape_id
            && !profile.capes.iter().any(|cape| cape.id == cape_id)
        {
            return Err(SkinCapeError::UnavailableCape);
        }
        if active_minecraft_cape_id(profile).as_deref() == target_cape_id {
            return Ok(None);
        }

        let request = match target_cape_id {
            Some(cape_id) => self
                .http
                .put(&self.endpoint)
                .json(&serde_json::json!({ "capeId": cape_id })),
            None => self.http.delete(&self.endpoint),
        };
        let response = request
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| SkinCapeError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SkinCapeError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinCapeError::RateLimited);
        }
        if status.is_client_error() {
            return Err(SkinCapeError::Rejected);
        }
        if status.is_server_error() {
            return Err(SkinCapeError::Unavailable);
        }

        let bytes =
            read_minecraft_profile_response(response)
                .await
                .map_err(|error| match error {
                    SkinUploadError::TooLarge => SkinCapeError::TooLarge,
                    SkinUploadError::Unavailable => SkinCapeError::Unavailable,
                    SkinUploadError::Auth => SkinCapeError::Auth,
                    SkinUploadError::RateLimited => SkinCapeError::RateLimited,
                    SkinUploadError::Rejected => SkinCapeError::Rejected,
                })?;
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(None);
        }

        let profile = serde_json::from_slice::<SkinUploadMinecraftProfile>(&bytes)
            .ok()
            .map(AuthLoginMinecraftProfile::from);
        Ok(profile)
    }
}

async fn read_skin_upload_response(
    response: reqwest::Response,
) -> Result<Vec<u8>, SkinUploadError> {
    read_minecraft_profile_response(response).await
}

async fn read_minecraft_profile_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, SkinUploadError> {
    if response
        .content_length()
        .is_some_and(|length| length > MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES as u64)
    {
        return Err(SkinUploadError::TooLarge);
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| SkinUploadError::Unavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES {
            return Err(SkinUploadError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

fn minecraft_skin_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(MINECRAFT_SKIN_HTTP_CONNECT_TIMEOUT)
                .timeout(MINECRAFT_SKIN_HTTP_TIMEOUT)
                .user_agent(CROOPOR_USER_AGENT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SkinUploadError {
    Auth,
    RateLimited,
    Rejected,
    TooLarge,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SkinCapeError {
    Auth,
    RateLimited,
    Rejected,
    TooLarge,
    Unavailable,
    UnavailableCape,
}

#[derive(Debug, Deserialize)]
struct SkinUploadMinecraftProfile {
    id: String,
    name: String,
    #[serde(default)]
    skins: Vec<SkinUploadMinecraftSkin>,
    #[serde(default)]
    capes: Vec<SkinUploadMinecraftCape>,
}

#[derive(Debug, Deserialize)]
struct SkinUploadMinecraftSkin {
    id: String,
    state: String,
    url: String,
    variant: String,
}

#[derive(Debug, Deserialize)]
struct SkinUploadMinecraftCape {
    id: String,
    state: String,
    url: String,
}

impl From<SkinUploadMinecraftProfile> for AuthLoginMinecraftProfile {
    fn from(profile: SkinUploadMinecraftProfile) -> Self {
        Self {
            id: profile.id,
            name: profile.name,
            skins: profile
                .skins
                .into_iter()
                .map(AuthLoginMinecraftSkin::from)
                .collect(),
            capes: profile
                .capes
                .into_iter()
                .map(AuthLoginMinecraftCape::from)
                .collect(),
        }
    }
}

impl From<SkinUploadMinecraftSkin> for AuthLoginMinecraftSkin {
    fn from(skin: SkinUploadMinecraftSkin) -> Self {
        Self {
            id: skin.id,
            state: skin.state,
            url: skin.url,
            variant: skin.variant,
        }
    }
}

impl From<SkinUploadMinecraftCape> for AuthLoginMinecraftCape {
    fn from(cape: SkinUploadMinecraftCape) -> Self {
        Self {
            id: cape.id,
            state: cape.state,
            url: cape.url,
        }
    }
}
