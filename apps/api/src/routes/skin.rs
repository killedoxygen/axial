use crate::state::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode, header},
    routing::get,
};
use croopor_config::validate_username;
use croopor_minecraft::offline_uuid;
use serde::{Deserialize, Serialize};
use std::fmt::Write;

const DEFAULT_HEAD_SIZE: u32 = 64;
const MIN_HEAD_SIZE: u32 = 16;
const MAX_HEAD_SIZE: u32 = 256;
const HEAD_CACHE_CONTROL: &str = "private, max-age=86400";

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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/skin/profile", get(handle_skin_profile))
        .route("/api/v1/skin/head", get(handle_skin_head))
}

async fn handle_skin_profile(
    State(state): State<AppState>,
    Query(query): Query<SkinQuery>,
) -> Result<Json<SkinProfileResponse>, (StatusCode, Json<serde_json::Value>)> {
    let config = state.config().current();
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

async fn handle_skin_head(
    State(state): State<AppState>,
    Query(query): Query<SkinQuery>,
) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
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

struct OfflineIdentity {
    username: String,
    uuid: String,
    variant: &'static str,
}

fn select_offline_identity(
    query_username: Option<&str>,
    config_username: &str,
) -> Result<OfflineIdentity, (StatusCode, Json<serde_json::Value>)> {
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
}
