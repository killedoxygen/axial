//! Profile texture delivery and provider mutations, with bounded responses.

use super::lookup::{ProfileLookup, ProfileMediaError, TextureUrl, http_client, read_response};
use crate::{
    accounts::microsoft::{
        MicrosoftAuthErrorKind, MinecraftProfile, MinecraftSkin, minecraft_profile,
    },
    media::{SkinVariant, normalize_cape_png, normalize_skin_png},
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct TextureDelivery {
    pub lookup: ProfileLookup,
    cache: Arc<Mutex<HashMap<(String, bool), Arc<[u8]>>>>,
}

impl TextureDelivery {
    pub fn new(lookup: ProfileLookup) -> Self {
        Self {
            lookup,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub(super) async fn fixture_skin(&self, url: &str, bytes: &[u8]) {
        let url = self.lookup.texture_url(url).unwrap();
        let png = normalize_skin_png(bytes).unwrap().png_bytes;
        self.cache
            .lock()
            .await
            .insert((url.as_str().to_owned(), false), Arc::from(png));
    }

    pub async fn skin(&self, url: &str) -> Result<Vec<u8>, ProfileMediaError> {
        self.texture(url, false).await
    }

    pub async fn cape(&self, url: &str) -> Result<Vec<u8>, ProfileMediaError> {
        self.texture(url, true).await
    }

    async fn texture(&self, url: &str, cape: bool) -> Result<Vec<u8>, ProfileMediaError> {
        let url = self.lookup.texture_url(url)?;
        let key = (url.as_str().to_owned(), cape);
        if let Some(bytes) = self.cache.lock().await.get(&key) {
            return Ok(bytes.to_vec());
        }
        let bytes = self.lookup.download(&url).await?;
        let bytes = if cape {
            normalize_cape_png(&bytes)
        } else {
            normalize_skin_png(&bytes).map(|skin| skin.png_bytes)
        }
        .map_err(|_| ProfileMediaError::InvalidImage)?;
        let mut cache = self.cache.lock().await;
        // Immutable texture URLs and a strict entry/byte bound keep this optional cache safe.
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, Arc::from(bytes.clone()));
        Ok(bytes)
    }
}

/// The skin a profile wears: its active one, or else the first with a usable texture.
pub fn current_skin(profile: &MinecraftProfile) -> Option<&MinecraftSkin> {
    let mut usable = profile
        .skins
        .iter()
        .filter(|skin| TextureUrl::parse(&skin.url).is_ok());
    let first = usable.next()?;
    std::iter::once(first)
        .chain(usable)
        .find(|skin| skin.state.eq_ignore_ascii_case("ACTIVE"))
        .or(Some(first))
}

pub fn profile_texture(profile: &MinecraftProfile) -> Result<&str, ProfileMediaError> {
    current_skin(profile)
        .map(|skin| skin.url.as_str())
        .ok_or(ProfileMediaError::MissingSkin)
}

pub fn active_cape(profile: &MinecraftProfile) -> Option<&str> {
    profile
        .capes
        .iter()
        .find(|cape| cape.state.eq_ignore_ascii_case("ACTIVE"))
        .map(|cape| cape.id.as_str())
}

#[derive(Clone)]
pub struct ProfileProvider {
    http: reqwest::Client,
    skins_endpoint: String,
    cape_endpoint: String,
    profile_endpoint: String,
}

impl ProfileProvider {
    pub fn new() -> Result<Self, ProfileMediaError> {
        Ok(Self {
            http: http_client()?,
            skins_endpoint: "https://api.minecraftservices.com/minecraft/profile/skins".into(),
            cape_endpoint: "https://api.minecraftservices.com/minecraft/profile/capes/active"
                .into(),
            profile_endpoint: "https://api.minecraftservices.com/minecraft/profile".into(),
        })
    }

    #[cfg(test)]
    pub(super) fn fixture(base: &str) -> Self {
        assert!(base.starts_with("http://127.0.0.1:"));
        Self {
            http: http_client().unwrap(),
            skins_endpoint: format!("{base}/skins"),
            cape_endpoint: format!("{base}/cape"),
            profile_endpoint: format!("{base}/profile"),
        }
    }

    pub async fn sync(
        &self,
        token: &str,
        expected_id: &str,
    ) -> Result<MinecraftProfile, ProfileMediaError> {
        // The sign-in owner fetches and validates profiles; this only maps its refusals.
        let profile = minecraft_profile(token, &self.profile_endpoint)
            .await
            .map_err(|error| match (error.kind(), error.provider_status()) {
                (_, Some(401 | 403)) => ProfileMediaError::AccountRequired,
                (_, Some(404)) => ProfileMediaError::NotFound,
                (_, Some(429)) => ProfileMediaError::RateLimited,
                (MicrosoftAuthErrorKind::UpstreamRejected, _) => ProfileMediaError::Rejected,
                (MicrosoftAuthErrorKind::Parse, _) => ProfileMediaError::InvalidResponse,
                _ => ProfileMediaError::Unavailable,
            })?;
        if !profile.id.eq_ignore_ascii_case(expected_id) {
            return Err(ProfileMediaError::InvalidResponse);
        }
        Ok(profile)
    }

    pub async fn upload(
        &self,
        token: &str,
        variant: SkinVariant,
        png: Vec<u8>,
        expected_profile_id: &str,
    ) -> Result<Option<MinecraftProfile>, ProfileMediaError> {
        let png = normalize_skin_png(&png)
            .map_err(|_| ProfileMediaError::InvalidImage)?
            .png_bytes;
        let part = reqwest::multipart::Part::bytes(png)
            .file_name("skin.png")
            .mime_str("image/png")
            .map_err(|_| ProfileMediaError::InvalidImage)?;
        let form = reqwest::multipart::Form::new()
            .text("variant", variant.as_str())
            .part("file", part);
        self.mutate(
            self.http
                .post(&self.skins_endpoint)
                .bearer_auth(token)
                .multipart(form),
            expected_profile_id,
        )
        .await
    }

    pub async fn reset_skin(
        &self,
        token: &str,
        expected_profile_id: &str,
    ) -> Result<Option<MinecraftProfile>, ProfileMediaError> {
        self.mutate(
            self.http
                .delete(format!("{}/active", self.skins_endpoint))
                .bearer_auth(token),
            expected_profile_id,
        )
        .await
    }

    pub async fn cape(
        &self,
        token: &str,
        profile: &MinecraftProfile,
        target: Option<&str>,
    ) -> Result<Option<MinecraftProfile>, ProfileMediaError> {
        if target.is_some_and(|id| !profile.capes.iter().any(|cape| cape.id == id)) {
            return Err(ProfileMediaError::MissingCape);
        }
        if active_cape(profile) == target {
            return Ok(None);
        }
        let request = match target {
            Some(id) => self
                .http
                .put(&self.cape_endpoint)
                .json(&serde_json::json!({ "capeId": id })),
            None => self.http.delete(&self.cape_endpoint),
        };
        self.mutate(request.bearer_auth(token), &profile.id).await
    }

    async fn mutate(
        &self,
        request: reqwest::RequestBuilder,
        expected_id: &str,
    ) -> Result<Option<MinecraftProfile>, ProfileMediaError> {
        // A send/read failure after mutation admission is not evidence that nothing changed.
        let response = request
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| ProfileMediaError::UncertainChange)?;
        let status = response.status();
        let bytes = read_response(response, 64 * 1024).await.map_err(|error| {
            if status.is_success() || status.is_server_error() {
                ProfileMediaError::UncertainChange
            } else {
                error
            }
        })?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let profile: MinecraftProfile =
            serde_json::from_slice(&bytes).map_err(|_| ProfileMediaError::UncertainChange)?;
        if !profile.id.eq_ignore_ascii_case(expected_id)
            || crate::accounts::microsoft::validate_profile(&profile).is_err()
        {
            return Err(ProfileMediaError::UncertainChange);
        }
        Ok(Some(profile))
    }
}
