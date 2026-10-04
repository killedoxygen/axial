//! Bounded Mojang profile lookup. Provider URLs never grant general network access.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const TEXTURE_PREFIX: &str = "https://textures.minecraft.net/texture/";
const JSON_LIMIT: usize = 64 * 1024;
pub const IMAGE_LIMIT: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfileMediaError {
    #[error("Minecraft username is invalid")]
    InvalidUsername,
    #[error("Minecraft texture URL is invalid")]
    InvalidTexture,
    #[error("Minecraft player was not found")]
    NotFound,
    #[error("Minecraft player has no usable skin texture")]
    MissingSkin,
    #[error("Minecraft cape is not available for this account")]
    MissingCape,
    #[error("Minecraft is limiting requests; try again later")]
    RateLimited,
    #[error("Minecraft account login required")]
    AccountRequired,
    #[error("This Microsoft account does not own Minecraft Java.")]
    OwnershipMissing,
    #[error(
        "The account changed before this operation completed; reload the account and try again"
    )]
    StaleIdentity,
    #[error("The queued skin change has changed; reload the wardrobe and try again")]
    StaleIntent,
    #[error("Minecraft rejected the profile change")]
    Rejected,
    #[error("Minecraft returned an invalid response")]
    InvalidResponse,
    #[error("Minecraft response exceeded the permitted size")]
    TooLarge,
    #[error("Minecraft is unavailable; try again later")]
    Unavailable,
    #[error("Skin image is invalid")]
    InvalidImage,
    #[error("Saved skin is unavailable")]
    SavedSkinMissing,
    #[error("The existing profile skin could not be preserved")]
    PreservationFailed,
    #[error("The skin changed but the cape change failed; sync the profile before trying again")]
    PartialChange,
    #[error("The provider may have changed the profile; sync it before trying again")]
    UncertainChange,
    #[error("The profile changed remotely but local state could not be saved; sync the profile")]
    SettlementFailed,
    #[error("Profile change canceled")]
    Cancelled,
    #[error("A profile change is already in progress for this account")]
    Busy,
    #[error("Profile changes are unavailable while shutting down")]
    ShuttingDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureUrl(String);

impl TextureUrl {
    pub fn parse(value: &str) -> Result<Self, ProfileMediaError> {
        Self::parse_prefix(value, TEXTURE_PREFIX)
    }

    fn parse_prefix(value: &str, prefix: &str) -> Result<Self, ProfileMediaError> {
        let canonical = if prefix.starts_with("https://") {
            value
                .strip_prefix("http://")
                .map(|rest| format!("https://{rest}"))
        } else {
            None
        };
        let value = canonical.as_deref().unwrap_or(value);
        let id = value
            .strip_prefix(prefix)
            .ok_or(ProfileMediaError::InvalidTexture)?;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(ProfileMediaError::InvalidTexture);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn validate_username(value: &str) -> Result<&str, ProfileMediaError> {
    let value = value.trim();
    if !(3..=16).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(ProfileMediaError::InvalidUsername);
    }
    Ok(value)
}

pub fn variant(value: &str) -> &'static str {
    if value.eq_ignore_ascii_case("slim") {
        "slim"
    } else {
        "classic"
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsernameProfile {
    pub uuid: String,
    pub username: String,
    pub source: &'static str,
    pub variant: String,
    pub texture_url: String,
    pub texture_file_url: String,
    pub cape_url: Option<String>,
    pub head_url: String,
}

#[derive(Clone)]
pub struct ProfileLookup {
    pub(crate) http: reqwest::Client,
    profile_endpoint: String,
    session_endpoint: String,
    texture_prefix: String,
    cache: Arc<Mutex<HashMap<String, (Instant, UsernameProfile)>>>,
}

impl ProfileLookup {
    pub fn new() -> Result<Self, ProfileMediaError> {
        Ok(Self {
            http: http_client()?,
            profile_endpoint: "https://api.mojang.com/users/profiles/minecraft".into(),
            session_endpoint: "https://sessionserver.mojang.com/session/minecraft/profile".into(),
            texture_prefix: TEXTURE_PREFIX.into(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    #[cfg(test)]
    pub(super) fn fixture(base: &str) -> Self {
        assert!(base.starts_with("http://127.0.0.1:"));
        Self {
            http: http_client().unwrap(),
            profile_endpoint: format!("{base}/names"),
            session_endpoint: format!("{base}/sessions"),
            texture_prefix: format!("{base}/texture/"),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn lookup(&self, username: &str) -> Result<UsernameProfile, ProfileMediaError> {
        let username = validate_username(username)?;
        let key = username.to_ascii_lowercase();
        if let Some((expires, result)) = self.cache.lock().await.get(&key) {
            if *expires > Instant::now() {
                return Ok(result.clone());
            }
        }
        let response = self
            .http
            .get(format!("{}/{username}", self.profile_endpoint))
            .send()
            .await
            .map_err(|_| ProfileMediaError::Unavailable)?;
        let profile: NameProfile =
            serde_json::from_slice(&read_response(response, 16 * 1024).await?)
                .map_err(|_| ProfileMediaError::InvalidResponse)?;
        if profile.id.len() != 32
            || !profile.id.bytes().all(|b| b.is_ascii_hexdigit())
            || validate_username(&profile.name).is_err()
        {
            return Err(ProfileMediaError::InvalidResponse);
        }
        let response = self
            .http
            .get(format!("{}/{}", self.session_endpoint, profile.id))
            .send()
            .await
            .map_err(|_| ProfileMediaError::Unavailable)?;
        let session: SessionProfile =
            serde_json::from_slice(&read_response(response, JSON_LIMIT).await?)
                .map_err(|_| ProfileMediaError::InvalidResponse)?;
        if session
            .id
            .as_deref()
            .is_some_and(|id| !id.eq_ignore_ascii_case(&profile.id))
        {
            return Err(ProfileMediaError::InvalidResponse);
        }
        let result = parse_textures(profile, session, &self.texture_prefix)?;
        let mut cache = self.cache.lock().await;
        cache.retain(|_, (expiry, _)| *expiry > Instant::now());
        if cache.len() >= 255 {
            cache.clear();
        }
        let entry = (Instant::now() + Duration::from_secs(300), result.clone());
        cache.insert(result.username.to_ascii_lowercase(), entry.clone());
        cache.insert(key, entry);
        Ok(result)
    }

    pub fn texture_url(&self, value: &str) -> Result<TextureUrl, ProfileMediaError> {
        TextureUrl::parse_prefix(value, &self.texture_prefix)
    }

    pub async fn download(&self, texture: &TextureUrl) -> Result<Vec<u8>, ProfileMediaError> {
        // Recheck even internal values: a fixture client cannot confer authority to another client.
        let texture = self.texture_url(texture.as_str())?;
        let response = self
            .http
            .get(texture.as_str())
            .header(reqwest::header::ACCEPT, "image/png")
            .send()
            .await
            .map_err(|_| ProfileMediaError::Unavailable)?;
        read_response(response, IMAGE_LIMIT).await
    }
}

pub(crate) fn http_client() -> Result<reqwest::Client, ProfileMediaError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .user_agent(concat!("axial/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| ProfileMediaError::Unavailable)
}

pub(crate) async fn read_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ProfileMediaError> {
    match response.status().as_u16() {
        200..=299 => (),
        401 | 403 => return Err(ProfileMediaError::AccountRequired),
        404 => return Err(ProfileMediaError::NotFound),
        429 => return Err(ProfileMediaError::RateLimited),
        400..=499 => return Err(ProfileMediaError::Rejected),
        _ => return Err(ProfileMediaError::Unavailable),
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ProfileMediaError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProfileMediaError::Unavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(ProfileMediaError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct NameProfile {
    id: String,
    name: String,
}
#[derive(Deserialize)]
struct SessionProfile {
    id: Option<String>,
    #[serde(default)]
    properties: Vec<Property>,
}
#[derive(Deserialize)]
struct Property {
    name: String,
    value: String,
}
#[derive(Deserialize)]
struct Textures {
    #[serde(rename = "profileId")]
    profile_id: Option<String>,
    textures: TextureMap,
}
#[derive(Deserialize)]
struct TextureMap {
    #[serde(rename = "SKIN")]
    skin: Option<Skin>,
    #[serde(rename = "CAPE")]
    cape: Option<Skin>,
}
#[derive(Deserialize)]
struct Skin {
    url: String,
    metadata: Option<SkinMetadata>,
}
#[derive(Deserialize)]
struct SkinMetadata {
    model: String,
}

fn parse_textures(
    profile: NameProfile,
    session: SessionProfile,
    prefix: &str,
) -> Result<UsernameProfile, ProfileMediaError> {
    let property = session
        .properties
        .into_iter()
        .find(|p| p.name == "textures")
        .ok_or(ProfileMediaError::MissingSkin)?;
    if property.value.len() > 16 * 1024 {
        return Err(ProfileMediaError::TooLarge);
    }
    let bytes = STANDARD
        .decode(property.value)
        .map_err(|_| ProfileMediaError::InvalidResponse)?;
    let textures: Textures =
        serde_json::from_slice(&bytes).map_err(|_| ProfileMediaError::InvalidResponse)?;
    if textures
        .profile_id
        .as_deref()
        .is_some_and(|id| !id.eq_ignore_ascii_case(&profile.id))
    {
        return Err(ProfileMediaError::InvalidResponse);
    }
    let skin = textures
        .textures
        .skin
        .ok_or(ProfileMediaError::MissingSkin)?;
    let texture_url = TextureUrl::parse_prefix(&skin.url, prefix)?.0;
    let cape_url = textures
        .textures
        .cape
        .and_then(|cape| TextureUrl::parse_prefix(&cape.url, prefix).ok())
        .map(|url| url.0);
    let model = skin
        .metadata
        .map(|m| variant(&m.model).to_owned())
        .unwrap_or_else(|| "classic".into());
    Ok(UsernameProfile {
        texture_file_url: format!("/api/v1/skin/lookup/file?username={}", profile.name),
        head_url: format!("/api/v1/skin/lookup/head?username={}", profile.name),
        uuid: profile.id,
        username: profile.name,
        source: "minecraft_username",
        variant: model,
        texture_url,
        cape_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_scope_accepts_legacy_http_but_rejects_authority_and_path_escape() {
        assert_eq!(
            TextureUrl::parse("http://textures.minecraft.net/texture/abc")
                .unwrap()
                .as_str(),
            "https://textures.minecraft.net/texture/abc"
        );
        for url in [
            "https://textures.minecraft.net.evil/texture/abc",
            "https://textures.minecraft.net/texture/../x",
            "https://textures.minecraft.net/texture/a?b",
            "https://textures.minecraft.net/texture/a#b",
            "https://textures.minecraft.net/texture/a%2fb",
            " https://textures.minecraft.net/texture/abc",
            "https://user@textures.minecraft.net/texture/abc",
            "http://127.0.0.1/texture/abc",
        ] {
            assert!(TextureUrl::parse(url).is_err(), "{url}");
        }
    }

    #[test]
    fn session_texture_identity_must_match_name_lookup() {
        let property = STANDARD.encode(br#"{"profileId":"different","textures":{"SKIN":{"url":"https://textures.minecraft.net/texture/abc"}}}"#);
        let session = SessionProfile {
            id: None,
            properties: vec![Property {
                name: "textures".into(),
                value: property,
            }],
        };
        assert_eq!(
            parse_textures(
                NameProfile {
                    id: "0123456789abcdef0123456789abcdef".into(),
                    name: "Alex".into()
                },
                session,
                TEXTURE_PREFIX
            )
            .unwrap_err(),
            ProfileMediaError::InvalidResponse
        );
    }
}
