//! Profile texture delivery and provider mutations, with bounded responses.

use super::lookup::{
    ProfileLookup, ProfileMediaError, TextureUrl, http_client, read_response, variant,
};
use crate::{
    accounts::microsoft::MinecraftProfile,
    media::{normalize_cape_png, normalize_skin_png, render_skin_head_png},
};
use serde::Serialize;
use std::{collections::HashMap, fmt::Write as _, sync::Arc};
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

    pub async fn head(&self, url: &str, size: Option<u32>) -> Result<Vec<u8>, ProfileMediaError> {
        let bytes = self.skin(url).await?;
        render_skin_head_png(&bytes, head_size(size)).map_err(|_| ProfileMediaError::InvalidImage)
    }
}

pub fn head_size(value: Option<u32>) -> u32 {
    value.unwrap_or(64).clamp(16, 256)
}

#[derive(Clone, Debug, Serialize)]
pub struct SkinProfileResponse {
    pub auth_mode: &'static str,
    pub username: String,
    pub uuid: String,
    pub source: &'static str,
    pub variant: &'static str,
    pub texture_url: Option<String>,
    pub head_url: Option<String>,
}

pub fn online_profile(profile: &MinecraftProfile) -> SkinProfileResponse {
    let selected = profile
        .skins
        .iter()
        .filter(|skin| TextureUrl::parse(&skin.url).is_ok())
        .find(|skin| skin.state.eq_ignore_ascii_case("ACTIVE"))
        .or_else(|| {
            profile
                .skins
                .iter()
                .find(|skin| TextureUrl::parse(&skin.url).is_ok())
        });
    SkinProfileResponse {
        auth_mode: "online",
        username: profile.name.clone(),
        uuid: profile.id.clone(),
        source: if selected.is_some() {
            "minecraft_profile_skin"
        } else {
            "default"
        },
        variant: selected
            .map(|skin| variant(&skin.variant))
            .unwrap_or("classic"),
        texture_url: selected
            .and_then(|skin| TextureUrl::parse(&skin.url).ok())
            .map(|url| url.as_str().to_owned()),
        head_url: None,
    }
}

pub fn profile_texture(profile: &MinecraftProfile) -> Result<&str, ProfileMediaError> {
    profile
        .skins
        .iter()
        .filter(|skin| TextureUrl::parse(&skin.url).is_ok())
        .find(|skin| skin.state.eq_ignore_ascii_case("ACTIVE"))
        .or_else(|| {
            profile
                .skins
                .iter()
                .find(|skin| TextureUrl::parse(&skin.url).is_ok())
        })
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

pub fn offline_profile(username: &str, uuid: &str) -> SkinProfileResponse {
    SkinProfileResponse {
        auth_mode: "offline",
        username: username.to_owned(),
        uuid: uuid.to_owned(),
        source: "default",
        variant: offline_variant(uuid),
        texture_url: None,
        head_url: Some(format!("/api/v1/skin/head?username={username}")),
    }
}

/// An imported Microsoft identity has no authenticated skin/profile yet.
pub fn unverified_profile(username: &str, uuid: &str) -> SkinProfileResponse {
    SkinProfileResponse {
        auth_mode: "online",
        username: username.to_owned(),
        uuid: uuid.to_owned(),
        source: "default",
        variant: offline_variant(uuid),
        texture_url: None,
        head_url: None,
    }
}

pub fn offline_variant(uuid: &str) -> &'static str {
    let hash = uuid.bytes().fold(0_i32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(i32::from(byte))
    });
    if hash & 1 == 0 { "classic" } else { "slim" }
}

/// Preserve the predecessor's deterministic offline avatar appearance.
pub fn offline_head_svg(uuid: &str, size: Option<u32>) -> String {
    let size = head_size(size);
    let seed = uuid.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    let palette = [
        mix_color(seed, 0x111827, 0x374151),
        mix_color(seed.rotate_left(7), 0x111827, 0x1f2937),
        mix_color(seed.rotate_left(17), 0xc58c65, 0xf1c27d),
        mix_color(seed.rotate_left(31), 0x2563eb, 0x22c55e),
        mix_color(seed.rotate_left(43), 0x4b5563, 0x7c2d12),
    ];
    let mut state = seed;
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 8 8" shape-rendering="crispEdges">"#
    );
    for y in 0..8 {
        for x in 0..8 {
            state = splitmix64(state.wrapping_add((y * 8 + x) as u64 + 1));
            let index = if x == 0 || x == 7 || y == 0 || y == 7 {
                1
            } else {
                (state as usize % 3) + 2
            };
            let _ = write!(
                svg,
                r##"<rect x="{x}" y="{y}" width="1" height="1" fill="#{:06x}"/>"##,
                palette[index]
            );
        }
    }
    svg.push_str("</svg>");
    svg
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn mix_color(seed: u64, first: u32, second: u32) -> u32 {
    let amount = (seed & 255) as u32;
    [16, 8, 0].into_iter().fold(0, |color, shift| {
        color
            | ((((first >> shift) & 255) * (255 - amount) + ((second >> shift) & 255) * amount)
                / 255
                << shift)
    })
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
        let response = self
            .http
            .get(&self.profile_endpoint)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| ProfileMediaError::Unavailable)?;
        let profile: MinecraftProfile =
            serde_json::from_slice(&read_response(response, 64 * 1024).await?)
                .map_err(|_| ProfileMediaError::InvalidResponse)?;
        if !profile.id.eq_ignore_ascii_case(expected_id)
            || crate::accounts::microsoft::validate_profile(&profile).is_err()
        {
            return Err(ProfileMediaError::InvalidResponse);
        }
        Ok(profile)
    }

    pub async fn upload(
        &self,
        token: &str,
        skin_variant: &str,
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
            .text("variant", variant(skin_variant).to_owned())
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_head_is_deterministic_and_size_bounded() {
        assert_eq!(
            offline_head_svg("player", Some(64)),
            offline_head_svg("player", Some(64))
        );
        assert_ne!(
            offline_head_svg("player", Some(64)),
            offline_head_svg("other", Some(64))
        );
        assert!(offline_head_svg("player", Some(u32::MAX)).contains("width=\"256\""));
        assert!(offline_head_svg("player", Some(0)).contains("width=\"16\""));
        assert_eq!(
            offline_head_svg("player", None).matches("<rect ").count(),
            64
        );
    }
}
