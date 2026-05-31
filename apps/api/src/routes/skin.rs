use crate::state::AppState;
use crate::state::{AuthLoginMinecraftAccount, AuthLoginMinecraftSkin};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Query, State},
    http::{Response, StatusCode, header},
    routing::{get, post},
};
use croopor_config::validate_username;
use croopor_minecraft::offline_uuid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt::Write, io::Cursor};

const DEFAULT_HEAD_SIZE: u32 = 64;
const MIN_HEAD_SIZE: u32 = 16;
const MAX_HEAD_SIZE: u32 = 256;
const HEAD_CACHE_CONTROL: &str = "private, max-age=86400";
const MINECRAFT_TEXTURE_URL_PREFIX: &str = "https://textures.minecraft.net/texture/";
const SKIN_UPLOAD_MAX_BYTES: usize = 256 * 1024;
const SKIN_WIDTH: u32 = 64;
const SKIN_HEIGHT: u32 = 64;
const LEGACY_SKIN_HEIGHT: u32 = 32;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Default, Deserialize)]
struct SkinQuery {
    username: Option<String>,
    size: Option<u32>,
}

#[derive(Debug, Serialize)]
struct SkinProfileResponse {
    auth_mode: &'static str,
    username: String,
    uuid: String,
    source: &'static str,
    variant: &'static str,
    texture_url: Option<String>,
    head_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct SkinNormalizeResponse {
    texture_key: String,
    variant_suggestion: &'static str,
    original_width: u32,
    original_height: u32,
    normalized_width: u32,
    normalized_height: u32,
    normalized_byte_size: usize,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/skin/profile", get(handle_skin_profile))
        .route("/api/v1/skin/head", get(handle_skin_head))
        .route("/api/v1/skins/normalize", post(handle_skin_normalize))
}

async fn handle_skin_profile(
    State(state): State<AppState>,
    Query(query): Query<SkinQuery>,
) -> Result<Json<SkinProfileResponse>, (StatusCode, Json<serde_json::Value>)> {
    let config = state.config().current();
    if query.username.is_none() {
        if let Some(profile) = online_skin_profile(
            state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await
                .map(|state| state.account),
        ) {
            return Ok(Json(profile));
        }
    }

    let identity = select_offline_identity(query.username.as_deref(), &config.username)?;

    Ok(Json(SkinProfileResponse {
        auth_mode: "offline",
        username: identity.username.clone(),
        uuid: identity.uuid,
        source: "default",
        variant: identity.variant,
        texture_url: None,
        head_url: Some(format!("/api/v1/skin/head?username={}", identity.username)),
    }))
}

fn online_skin_profile(account: Option<AuthLoginMinecraftAccount>) -> Option<SkinProfileResponse> {
    let account = account?;
    let profile_name = account.profile.name.trim();
    let profile_id = account.profile.id.trim();
    if profile_name.is_empty() || profile_id.is_empty() {
        return None;
    }

    let selected_skin = select_minecraft_skin(&account.profile.skins);
    let texture_url = selected_skin.and_then(|skin| sane_minecraft_texture_url(&skin.url));
    let variant = selected_skin
        .map(|skin| skin_variant(&skin.variant))
        .unwrap_or("classic");
    let source = if selected_skin.is_some() {
        "minecraft_profile_skin"
    } else {
        "default"
    };

    Some(SkinProfileResponse {
        auth_mode: "online",
        username: profile_name.to_string(),
        uuid: profile_id.to_string(),
        source,
        variant,
        texture_url,
        head_url: None,
    })
}

fn select_minecraft_skin(skins: &[AuthLoginMinecraftSkin]) -> Option<&AuthLoginMinecraftSkin> {
    skins
        .iter()
        .find(|skin| skin.state.eq_ignore_ascii_case("ACTIVE"))
        .or_else(|| {
            skins
                .iter()
                .find(|skin| sane_minecraft_texture_url(&skin.url).is_some())
        })
}

fn skin_variant(variant: &str) -> &'static str {
    if variant.eq_ignore_ascii_case("slim") {
        "slim"
    } else {
        "classic"
    }
}

fn sane_minecraft_texture_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed != url || !trimmed.starts_with(MINECRAFT_TEXTURE_URL_PREFIX) {
        return None;
    }

    let texture_id = &trimmed[MINECRAFT_TEXTURE_URL_PREFIX.len()..];
    if texture_id.is_empty() || texture_id.len() > 128 {
        return None;
    }
    if !texture_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return None;
    }

    Some(trimmed.to_string())
}

async fn handle_skin_head(
    State(state): State<AppState>,
    Query(query): Query<SkinQuery>,
) -> Result<Response<Body>, ApiError> {
    let config = state.config().current();
    let identity = select_offline_identity(query.username.as_deref(), &config.username)?;
    let size = clamp_head_size(query.size);
    let svg = offline_head_svg(&identity.uuid, size);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/svg+xml")
        .header(header::CACHE_CONTROL, HEAD_CACHE_CONTROL)
        .body(Body::from(svg))
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to build skin head response" })),
            )
        })
}

async fn handle_skin_normalize(body: Body) -> Result<Json<SkinNormalizeResponse>, ApiError> {
    let bytes = to_bytes(body, SKIN_UPLOAD_MAX_BYTES)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "skin upload is too large"))?;
    let normalized = normalize_skin_png(&bytes)?;

    Ok(Json(SkinNormalizeResponse {
        texture_key: texture_key(&normalized.png_bytes),
        variant_suggestion: "classic",
        original_width: normalized.original_width,
        original_height: normalized.original_height,
        normalized_width: SKIN_WIDTH,
        normalized_height: SKIN_HEIGHT,
        normalized_byte_size: normalized.png_bytes.len(),
    }))
}

struct NormalizedSkinPng {
    original_width: u32,
    original_height: u32,
    png_bytes: Vec<u8>,
}

fn normalize_skin_png(bytes: &[u8]) -> Result<NormalizedSkinPng, ApiError> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin upload must be a PNG",
        ));
    }

    let decoded = decode_skin_png(bytes)?;
    if decoded.width != SKIN_WIDTH || !matches!(decoded.height, LEGACY_SKIN_HEIGHT | SKIN_HEIGHT) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin image must be 64x64 or 64x32",
        ));
    }

    let normalized_rgba = if decoded.height == LEGACY_SKIN_HEIGHT {
        normalize_legacy_skin_rgba(&decoded.rgba)
    } else {
        decoded.rgba
    };
    let png_bytes = encode_skin_png(&normalized_rgba)?;

    Ok(NormalizedSkinPng {
        original_width: decoded.width,
        original_height: decoded.height,
        png_bytes,
    })
}

struct DecodedSkinPng {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn decode_skin_png(bytes: &[u8]) -> Result<DecodedSkinPng, ApiError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "skin upload must be a valid PNG"))?;
    let info = reader.info();
    if info.width != SKIN_WIDTH || !matches!(info.height, LEGACY_SKIN_HEIGHT | SKIN_HEIGHT) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin image must be 64x64 or 64x32",
        ));
    }

    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "skin upload must be a valid PNG"))?;
    let rgba = png_frame_to_rgba(
        &buffer[..frame.buffer_size()],
        frame.color_type,
        frame.bit_depth,
    )?;

    Ok(DecodedSkinPng {
        width: frame.width,
        height: frame.height,
        rgba,
    })
}

fn png_frame_to_rgba(
    data: &[u8],
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
) -> Result<Vec<u8>, ApiError> {
    if bit_depth != png::BitDepth::Eight {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin upload must be a valid PNG",
        ));
    }

    match color_type {
        png::ColorType::Rgba => Ok(data.to_vec()),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(data.len() / 3 * 4);
            for pixel in data.chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            Ok(rgba)
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(data.len() / 2 * 4);
            for pixel in data.chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            Ok(rgba)
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(data.len() * 4);
            for value in data {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
            Ok(rgba)
        }
        png::ColorType::Indexed => Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin upload must be a valid PNG",
        )),
    }
}

fn normalize_legacy_skin_rgba(rgba: &[u8]) -> Vec<u8> {
    let mut normalized = vec![0; (SKIN_WIDTH * SKIN_HEIGHT * 4) as usize];
    let row_len = (SKIN_WIDTH * 4) as usize;
    for row in 0..LEGACY_SKIN_HEIGHT as usize {
        let offset = row * row_len;
        normalized[offset..offset + row_len].copy_from_slice(&rgba[offset..offset + row_len]);
    }
    normalized
}

fn encode_skin_png(rgba: &[u8]) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, SKIN_WIDTH, SKIN_HEIGHT);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to normalize skin image",
            )
        })?;
        writer.write_image_data(rgba).map_err(|_| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to normalize skin image",
            )
        })?;
    }
    Ok(bytes)
}

fn texture_key(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut key = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut key, "{byte:02x}").expect("write sha256 hex");
    }
    key
}

struct OfflineIdentity {
    username: String,
    uuid: String,
    variant: &'static str,
}

fn select_offline_identity(
    query_username: Option<&str>,
    config_username: &str,
) -> Result<OfflineIdentity, ApiError> {
    let selected_username = query_username
        .map(str::trim)
        .filter(|username| !username.is_empty())
        .unwrap_or(config_username);
    let username = validate_username(selected_username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let uuid = offline_uuid(&username);
    let variant = offline_variant(&uuid);

    Ok(OfflineIdentity {
        username,
        uuid,
        variant,
    })
}

fn json_error(status: StatusCode, message: &'static str) -> ApiError {
    (status, Json(serde_json::json!({ "error": message })))
}

fn offline_variant(uuid: &str) -> &'static str {
    // Mirrors Java String.hashCode parity so the offline hint is stable across platforms.
    let hash = uuid.bytes().fold(0_i32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(i32::from(byte))
    });
    if hash & 1 == 0 { "classic" } else { "slim" }
}

fn clamp_head_size(size: Option<u32>) -> u32 {
    size.unwrap_or(DEFAULT_HEAD_SIZE)
        .clamp(MIN_HEAD_SIZE, MAX_HEAD_SIZE)
}

fn offline_head_svg(uuid: &str, size: u32) -> String {
    let seed = fnv1a64(uuid.as_bytes());
    let background = mix_color(seed, 0x111827, 0x374151);
    let outline = mix_color(seed.rotate_left(7), 0x111827, 0x1f2937);
    let skin = mix_color(seed.rotate_left(17), 0xc58c65, 0xf1c27d);
    let accent = mix_color(seed.rotate_left(31), 0x2563eb, 0x22c55e);
    let shadow = mix_color(seed.rotate_left(43), 0x4b5563, 0x7c2d12);
    let palette = [background, outline, skin, accent, shadow];
    let mut state = seed;

    let mut svg = String::with_capacity(2600);
    write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 8 8" shape-rendering="crispEdges">"#
    )
    .expect("write svg header");

    for y in 0..8 {
        for x in 0..8 {
            state = splitmix64(state.wrapping_add(((y * 8 + x) as u64) + 1));
            let palette_index = if x == 0 || x == 7 || y == 0 || y == 7 {
                1
            } else {
                (state as usize % (palette.len() - 2)) + 2
            };
            write!(
                svg,
                r##"<rect x="{x}" y="{y}" width="1" height="1" fill="#{:06x}"/>"##,
                palette[palette_index]
            )
            .expect("write svg pixel");
        }
    }

    svg.push_str("</svg>");
    svg
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn mix_color(seed: u64, first: u32, second: u32) -> u32 {
    let amount = (seed & 0xff) as u32;
    let inverse = 255 - amount;
    let red = (((first >> 16) & 0xff) * inverse + ((second >> 16) & 0xff) * amount) / 255;
    let green = (((first >> 8) & 0xff) * inverse + ((second >> 8) & 0xff) * amount) / 255;
    let blue = ((first & 0xff) * inverse + (second & 0xff) * amount) / 255;

    (red << 16) | (green << 8) | blue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use crate::state::{
        AuthLoginMinecraftProfile, AuthLoginMinecraftSkin, NewAuthLoginMinecraftAccount,
        NewAuthLoginMsaToken, NewAuthLoginSession,
    };
    use axum::body::to_bytes;
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{fs, path::PathBuf, sync::Arc};

    #[tokio::test]
    async fn skin_profile_defaults_to_configured_username() {
        let fixture = TestFixture::new("default-username", "ConfigUser");

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.source, "default");
        assert_eq!(response.texture_url, None);
        assert_eq!(
            response.head_url,
            Some("/api/v1/skin/head?username=ConfigUser".to_string())
        );
    }

    #[tokio::test]
    async fn skin_profile_query_username_overrides_config_username() {
        let fixture = TestFixture::new("query-username", "ConfigUser");

        let response = fixture
            .profile(Some("QueryUser".to_string()), None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.username, "QueryUser");
        assert_eq!(response.uuid, offline_uuid("QueryUser"));
    }

    #[tokio::test]
    async fn skin_profile_blank_username_falls_back_to_config_username() {
        let fixture = TestFixture::new("blank-username", "ConfigUser");

        let response = fixture
            .profile(Some("   ".to_string()), None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
    }

    #[tokio::test]
    async fn skin_profile_uses_active_minecraft_profile_when_no_username_query() {
        let fixture = TestFixture::new("online-profile", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![
                    minecraft_skin(
                        "inactive",
                        "INACTIVE",
                        "https://textures.minecraft.net/texture/inactive",
                        "classic",
                    ),
                    minecraft_skin(
                        "active",
                        "ACTIVE",
                        "https://textures.minecraft.net/texture/activeTexture123",
                        "SLIM",
                    ),
                ],
            ))
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "online");
        assert_eq!(response.username, "MinecraftName");
        assert_eq!(response.uuid, "MinecraftName-id");
        assert_eq!(response.source, "minecraft_profile_skin");
        assert_eq!(response.variant, "slim");
        assert_eq!(
            response.texture_url.as_deref(),
            Some("https://textures.minecraft.net/texture/activeTexture123")
        );
        assert_eq!(response.head_url, None);
    }

    #[tokio::test]
    async fn skin_profile_ignores_preserved_stale_minecraft_profile() {
        let fixture = TestFixture::new("online-profile-stale", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "OldMinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/oldTexture123",
                    "slim",
                )],
            ))
            .await;
        fixture
            .state
            .auth_logins()
            .refresh_with_msa_token(
                NewAuthLoginMsaToken {
                    access_token: "new-msa-access-token".to_string(),
                    refresh_token: Some("new-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                "old-msa-refresh-token",
            )
            .await
            .expect("msa-only refresh");

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.source, "default");
        assert_eq!(
            response.variant,
            offline_variant(&offline_uuid("ConfigUser"))
        );
        assert_eq!(response.texture_url, None);
        assert_eq!(
            response.head_url,
            Some("/api/v1/skin/head?username=ConfigUser".to_string())
        );
        assert_eq!(
            fixture
                .state
                .auth_logins()
                .active_minecraft_account()
                .await
                .expect("preserved raw minecraft account")
                .profile
                .name,
            "OldMinecraftName"
        );
        assert_eq!(
            fixture
                .state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await,
            None
        );
    }

    #[tokio::test]
    async fn skin_profile_username_query_keeps_offline_override_with_active_minecraft_profile() {
        let fixture = TestFixture::new("online-query-override", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/active",
                    "slim",
                )],
            ))
            .await;

        let response = fixture
            .profile(Some("QueryUser".to_string()), None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "QueryUser");
        assert_eq!(response.uuid, offline_uuid("QueryUser"));
        assert_eq!(response.texture_url, None);
    }

    #[tokio::test]
    async fn skin_profile_expired_minecraft_profile_falls_back_to_offline() {
        let fixture = TestFixture::new("online-expired", "ConfigUser");
        fixture
            .add_minecraft_account_with_expiry(
                test_profile(
                    "MinecraftName",
                    vec![minecraft_skin(
                        "active",
                        "ACTIVE",
                        "https://textures.minecraft.net/texture/active",
                        "slim",
                    )],
                ),
                0,
            )
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(
            fixture.state.auth_logins().active_minecraft_account().await,
            None
        );
    }

    #[tokio::test]
    async fn skin_profile_omits_unsane_minecraft_texture_url() {
        let fixture = TestFixture::new("online-bad-texture", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://example.com/texture/active",
                    "unknown",
                )],
            ))
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "online");
        assert_eq!(response.source, "minecraft_profile_skin");
        assert_eq!(response.variant, "classic");
        assert_eq!(response.texture_url, None);
    }

    #[tokio::test]
    async fn skin_profile_without_active_skin_uses_first_sane_skin() {
        let fixture = TestFixture::new("online-first-sane-texture", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![
                    minecraft_skin("bad", "INACTIVE", "https://example.com/texture/bad", "slim"),
                    minecraft_skin(
                        "good",
                        "INACTIVE",
                        "https://textures.minecraft.net/texture/goodTexture123",
                        "classic",
                    ),
                ],
            ))
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.source, "minecraft_profile_skin");
        assert_eq!(response.variant, "classic");
        assert_eq!(
            response.texture_url.as_deref(),
            Some("https://textures.minecraft.net/texture/goodTexture123")
        );
    }

    #[test]
    fn minecraft_texture_url_sanitization_is_strict() {
        assert_eq!(
            sane_minecraft_texture_url("https://textures.minecraft.net/texture/abcDEF123"),
            Some("https://textures.minecraft.net/texture/abcDEF123".to_string())
        );
        assert_eq!(
            sane_minecraft_texture_url("http://textures.minecraft.net/texture/abc"),
            None
        );
        assert_eq!(
            sane_minecraft_texture_url("https://textures.minecraft.net.evil/texture/abc"),
            None
        );
        assert_eq!(
            sane_minecraft_texture_url("https://textures.minecraft.net/texture/abc?token=secret"),
            None
        );
        assert_eq!(
            sane_minecraft_texture_url(" https://textures.minecraft.net/texture/abc"),
            None
        );
    }

    #[tokio::test]
    async fn skin_profile_invalid_username_returns_json_error() {
        let fixture = TestFixture::new("invalid-username", "ConfigUser");

        let error = fixture
            .profile(Some("bad name".to_string()), None)
            .await
            .expect_err("invalid username should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "Letters, numbers, and underscores only." })
        );
    }

    #[test]
    fn offline_variant_is_deterministic_and_known() {
        let uuid = offline_uuid("ConfigUser");

        let first = offline_variant(&uuid);
        let second = offline_variant(&uuid);

        assert_eq!(first, second);
        assert!(matches!(first, "classic" | "slim"));
    }

    #[tokio::test]
    async fn skin_head_defaults_to_configured_username() {
        let fixture = TestFixture::new("head-default-username", "ConfigUser");

        let response = fixture.head(None, None).await.expect("head response");
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let cache_control = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response_body(response).await;

        assert_eq!(content_type.as_deref(), Some("image/svg+xml"));
        assert_eq!(cache_control.as_deref(), Some(HEAD_CACHE_CONTROL));
        assert!(body.contains("<svg"));
        assert_eq!(
            body,
            offline_head_svg(&offline_uuid("ConfigUser"), DEFAULT_HEAD_SIZE)
        );
    }

    #[tokio::test]
    async fn skin_head_query_username_overrides_config_username() {
        let fixture = TestFixture::new("head-query-username", "ConfigUser");

        let default_response = fixture.head(None, None).await.expect("default head");
        let query_response = fixture
            .head(Some("QueryUser".to_string()), None)
            .await
            .expect("query head");

        assert_ne!(
            response_body(default_response).await,
            response_body(query_response).await
        );
    }

    #[tokio::test]
    async fn skin_head_blank_username_falls_back_to_config_username() {
        let fixture = TestFixture::new("head-blank-username", "ConfigUser");

        let default_response = fixture.head(None, None).await.expect("default head");
        let blank_response = fixture
            .head(Some("   ".to_string()), None)
            .await
            .expect("blank head");

        assert_eq!(
            response_body(default_response).await,
            response_body(blank_response).await
        );
    }

    #[tokio::test]
    async fn skin_head_invalid_username_returns_json_error() {
        let fixture = TestFixture::new("head-invalid-username", "ConfigUser");

        let error = fixture
            .head(Some("bad name".to_string()), None)
            .await
            .expect_err("invalid username should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "Letters, numbers, and underscores only." })
        );
    }

    #[tokio::test]
    async fn skin_head_size_clamps_to_sane_bounds() {
        let fixture = TestFixture::new("head-size-clamps", "ConfigUser");

        let small_response = fixture.head(None, Some(1)).await.expect("small head");
        let large_response = fixture.head(None, Some(9999)).await.expect("large head");

        assert!(
            response_body(small_response)
                .await
                .contains(r#"width="16""#)
        );
        assert!(
            response_body(large_response)
                .await
                .contains(r#"width="256""#)
        );
    }

    #[tokio::test]
    async fn skin_normalize_64x64_png_succeeds() {
        let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);

        let response = normalize_skin_body(png)
            .await
            .expect("normalize response")
            .0;

        assert_eq!(response.variant_suggestion, "classic");
        assert_eq!(response.original_width, SKIN_WIDTH);
        assert_eq!(response.original_height, SKIN_HEIGHT);
        assert_eq!(response.normalized_width, SKIN_WIDTH);
        assert_eq!(response.normalized_height, SKIN_HEIGHT);
        assert!(response.normalized_byte_size > 0);
        assert_texture_key(&response.texture_key);
    }

    #[tokio::test]
    async fn skin_normalize_64x32_png_normalizes_to_64x64() {
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);

        let response = normalize_skin_body(png.clone())
            .await
            .expect("normalize response")
            .0;
        let repeated = normalize_skin_body(png)
            .await
            .expect("repeat normalize response")
            .0;

        assert_eq!(response.original_width, SKIN_WIDTH);
        assert_eq!(response.original_height, LEGACY_SKIN_HEIGHT);
        assert_eq!(response.normalized_width, SKIN_WIDTH);
        assert_eq!(response.normalized_height, SKIN_HEIGHT);
        assert_eq!(response.texture_key, repeated.texture_key);
        assert_eq!(response.normalized_byte_size, repeated.normalized_byte_size);
        assert_texture_key(&response.texture_key);
    }

    #[tokio::test]
    async fn skin_normalize_rejects_non_png() {
        let error = normalize_skin_body(b"/home/zero/not-a-skin".to_vec())
            .await
            .expect_err("non-png should fail");

        assert_skin_normalize_error(error, StatusCode::BAD_REQUEST, "skin upload must be a PNG");
    }

    #[tokio::test]
    async fn skin_normalize_rejects_bad_dimensions() {
        let error = normalize_skin_body(test_skin_png(32, 32))
            .await
            .expect_err("bad dimensions should fail");

        assert_skin_normalize_error(
            error,
            StatusCode::BAD_REQUEST,
            "skin image must be 64x64 or 64x32",
        );
    }

    #[tokio::test]
    async fn skin_normalize_rejects_malformed_png_with_bounded_error() {
        let mut body = PNG_SIGNATURE.to_vec();
        body.extend_from_slice(b"/home/zero/corrupt-skin");

        let error = normalize_skin_body(body)
            .await
            .expect_err("malformed png should fail");

        assert_skin_normalize_error(
            error,
            StatusCode::BAD_REQUEST,
            "skin upload must be a valid PNG",
        );
    }

    #[tokio::test]
    async fn skin_normalize_rejects_oversized_body() {
        let error = normalize_skin_body(vec![0; SKIN_UPLOAD_MAX_BYTES + 1])
            .await
            .expect_err("oversized body should fail");

        assert_skin_normalize_error(
            error,
            StatusCode::PAYLOAD_TOO_LARGE,
            "skin upload is too large",
        );
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str, username: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            config
                .replace_in_memory(AppConfig {
                    username: username.to_string(),
                    ..AppConfig::default()
                })
                .expect("set username");
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }

        async fn profile(
            &self,
            username: Option<String>,
            size: Option<u32>,
        ) -> Result<Json<SkinProfileResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_profile(
                State(self.state.clone()),
                Query(SkinQuery { username, size }),
            )
            .await
        }

        async fn head(
            &self,
            username: Option<String>,
            size: Option<u32>,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_head(
                State(self.state.clone()),
                Query(SkinQuery { username, size }),
            )
            .await
        }

        async fn add_minecraft_account(&self, profile: AuthLoginMinecraftProfile) {
            self.add_minecraft_account_with_expiry(profile, 86_400)
                .await;
        }

        async fn add_minecraft_account_with_expiry(
            &self,
            profile: AuthLoginMinecraftProfile,
            expires_in: u64,
        ) {
            let session = self
                .state
                .auth_logins()
                .insert(NewAuthLoginSession {
                    device_code: "raw-device-code".to_string(),
                    user_code: "ABCD-EFGH".to_string(),
                    verification_uri: "https://www.microsoft.com/link".to_string(),
                    expires_in: 900,
                    interval: 5,
                    message: None,
                })
                .await;

            self.state
                .auth_logins()
                .complete_with_msa_and_minecraft_account(
                    &session.login_id,
                    NewAuthLoginMsaToken {
                        access_token: "msa-access-token".to_string(),
                        refresh_token: Some("msa-refresh-token".to_string()),
                        id_token: None,
                        token_type: "Bearer".to_string(),
                        expires_in: 3600,
                        scope: Some("XboxLive.signin offline_access".to_string()),
                    },
                    NewAuthLoginMinecraftAccount {
                        access_token: "minecraft-access-token".to_string(),
                        token_type: Some("Bearer".to_string()),
                        expires_in,
                        profile,
                        owns_minecraft_java: true,
                    },
                )
                .await
                .expect("complete minecraft account login");
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-skin-{name}-{}-{}",
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

    async fn normalize_skin_body(
        body: Vec<u8>,
    ) -> Result<Json<SkinNormalizeResponse>, (StatusCode, Json<serde_json::Value>)> {
        handle_skin_normalize(Body::from(body)).await
    }

    fn test_skin_png(width: u32, height: u32) -> Vec<u8> {
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

        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write png header");
            writer.write_image_data(&rgba).expect("write png pixels");
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
        AuthLoginMinecraftProfile {
            id: format!("{name}-id"),
            name: name.to_string(),
            skins,
            capes: vec![],
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
}
