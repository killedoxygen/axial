use super::*;

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
    assert!(
        fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .is_none()
    );
    assert!(
        fixture
            .state
            .auth_logins()
            .active_minecraft_account()
            .await
            .is_some()
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

#[tokio::test]
async fn skin_profile_file_downloads_normalizes_active_skin() {
    let fixture = TestFixture::new("profile-file-active", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![
                minecraft_skin(
                    "inactive",
                    "INACTIVE",
                    &format!("{texture_prefix}inactiveTexture123"),
                    "classic",
                ),
                minecraft_skin(
                    "active",
                    "ACTIVE",
                    &format!("{texture_prefix}activeTexture123"),
                    "slim",
                ),
            ],
        ))
        .await;

    let file = fixture
        .profile_file(texture_prefix.clone())
        .await
        .expect("profile skin file");
    let request = requests.recv().await.expect("texture request");
    let content_type = file
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let cache_control = file
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    assert_eq!(request.path, "/texture/activeTexture123");
    assert_eq!(request.accept.as_deref(), Some("image/png"));
    assert_eq!(request.user_agent.as_deref(), Some(AXIAL_USER_AGENT));
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(
        cache_control.as_deref(),
        Some(PROFILE_SKIN_FILE_CACHE_CONTROL)
    );
    assert_eq!(response_bytes(file).await, normalized.png_bytes);
}

#[tokio::test]
async fn skin_profile_file_texture_query_fetches_requested_profile_texture() {
    let fixture = TestFixture::new("profile-file-query-texture", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let active_texture_url = format!("{texture_prefix}activeTexture123");
    let requested_texture_url = format!("{texture_prefix}otherAccountTexture456");
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin(
                "active",
                "ACTIVE",
                &active_texture_url,
                "slim",
            )],
        ))
        .await;

    let file = fixture
        .profile_file_with_texture(texture_prefix.clone(), Some(requested_texture_url.clone()))
        .await
        .expect("profile skin file");
    let request = requests.recv().await.expect("texture request");
    let cache_path = profile_skin_file_cache_path(
        fixture.state.config().paths().skins_dir(),
        &requested_texture_url,
    );

    assert_eq!(request.path, "/texture/otherAccountTexture456");
    assert_eq!(response_bytes(file).await, normalized.png_bytes);
    assert_eq!(
        tokio::fs::read(cache_path)
            .await
            .expect("read requested profile cache"),
        normalized.png_bytes
    );
}

#[tokio::test]
async fn skin_profile_file_cache_hit_avoids_second_texture_request() {
    let fixture = TestFixture::new("profile-file-cache-hit", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let texture_url = format!("{texture_prefix}activeTexture123");
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin("active", "ACTIVE", &texture_url, "slim")],
        ))
        .await;

    let first = fixture
        .profile_file(texture_prefix.clone())
        .await
        .expect("first profile skin file");
    let request = requests.recv().await.expect("texture request");
    let second = fixture
        .profile_file(texture_prefix)
        .await
        .expect("second profile skin file");
    let cache_path =
        profile_skin_file_cache_path(fixture.state.config().paths().skins_dir(), &texture_url);

    assert_eq!(request.path, "/texture/activeTexture123");
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(response_bytes(first).await, normalized.png_bytes);
    assert_eq!(response_bytes(second).await, normalized.png_bytes);
    assert_eq!(
        tokio::fs::read(cache_path)
            .await
            .expect("read profile cache"),
        normalized.png_bytes
    );
}

#[tokio::test]
async fn skin_profile_file_corrupt_cache_redownloads_and_refreshes_cache() {
    let fixture = TestFixture::new("profile-file-corrupt-cache", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let texture_url = format!("{texture_prefix}activeTexture123");
    let cache_path =
        profile_skin_file_cache_path(fixture.state.config().paths().skins_dir(), &texture_url);
    tokio::fs::create_dir_all(cache_path.parent().expect("profile cache parent"))
        .await
        .expect("create profile cache dir");
    tokio::fs::write(&cache_path, b"\x89PNG\r\n\x1a\n/home/zero/corrupt-cache")
        .await
        .expect("write corrupt profile cache");
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin("active", "ACTIVE", &texture_url, "slim")],
        ))
        .await;

    let file = fixture
        .profile_file(texture_prefix)
        .await
        .expect("profile skin file");
    let request = requests.recv().await.expect("texture request");

    assert_eq!(request.path, "/texture/activeTexture123");
    assert_eq!(response_bytes(file).await, normalized.png_bytes);
    assert_eq!(
        tokio::fs::read(cache_path)
            .await
            .expect("read refreshed cache"),
        normalized.png_bytes
    );
}

#[tokio::test]
async fn skin_cape_file_downloads_available_account_cape() {
    let fixture = TestFixture::new("cape-file-download", "ConfigUser");
    let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone())).await;
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            Vec::new(),
            vec![minecraft_cape(
                "cape-id",
                "INACTIVE",
                &format!("{texture_prefix}capeTexture123"),
            )],
        ))
        .await;

    let file = fixture
        .cape_file("cape-id", texture_prefix.clone())
        .await
        .expect("profile cape file");
    let request = requests.recv().await.expect("cape texture request");
    let content_type = file
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let cache_control = file
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    assert_eq!(request.path, "/texture/capeTexture123");
    assert_eq!(request.accept.as_deref(), Some("image/png"));
    assert_eq!(request.user_agent.as_deref(), Some(AXIAL_USER_AGENT));
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(
        cache_control.as_deref(),
        Some(PROFILE_CAPE_FILE_CACHE_CONTROL)
    );
    assert_eq!(response_bytes(file).await, cape_png);
}

#[tokio::test]
async fn skin_cape_file_cache_hit_avoids_second_texture_request() {
    let fixture = TestFixture::new("cape-file-cache-hit", "ConfigUser");
    let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone())).await;
    let texture_url = format!("{texture_prefix}capeTexture123");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            Vec::new(),
            vec![minecraft_cape("cape-id", "INACTIVE", &texture_url)],
        ))
        .await;

    let first = fixture
        .cape_file("cape-id", texture_prefix.clone())
        .await
        .expect("first profile cape file");
    let request = requests.recv().await.expect("cape texture request");
    let second = fixture
        .cape_file("cape-id", texture_prefix)
        .await
        .expect("second profile cape file");
    let cache_path =
        profile_cape_file_cache_path(fixture.state.config().paths().skins_dir(), &texture_url);

    assert_eq!(request.path, "/texture/capeTexture123");
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(response_bytes(first).await, cape_png);
    assert_eq!(response_bytes(second).await, cape_png);
    assert_eq!(
        tokio::fs::read(cache_path).await.expect("read cape cache"),
        cape_png
    );
}

#[tokio::test]
async fn skin_cape_file_requires_available_sane_cape_texture() {
    let fixture = TestFixture::new("cape-file-bad-texture", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            Vec::new(),
            vec![minecraft_cape(
                "cape-id",
                "INACTIVE",
                "https://example.com/texture/capeTexture123",
            )],
        ))
        .await;

    let error = fixture
        .cape_file("cape-id", "http://127.0.0.1:9/texture/".to_string())
        .await
        .expect_err("bad cape texture should fail");
    let missing = fixture
        .cape_file("missing-cape", "http://127.0.0.1:9/texture/".to_string())
        .await
        .expect_err("missing cape should fail");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft cape does not have a usable texture",
            "status": "minecraft_cape_texture_missing",
        })
    );
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_eq!(
        missing.1.0,
        serde_json::json!({
            "error": "Minecraft cape is not available for this account",
            "status": "minecraft_cape_not_found",
        })
    );
}

#[tokio::test]
async fn skin_profile_file_missing_active_account_returns_bounded_error() {
    let fixture = TestFixture::new("profile-file-missing-active", "ConfigUser");

    let error = fixture
        .profile_file("http://127.0.0.1:9/texture/".to_string())
        .await
        .expect_err("missing active account should fail");

    assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft account login required",
            "status": "minecraft_account_required",
        })
    );
}

#[tokio::test]
async fn skin_profile_file_not_ready_account_returns_bounded_error() {
    let fixture = TestFixture::new("profile-file-not-ready", "ConfigUser");
    fixture
        .add_minecraft_account_with_ownership(
            test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/activeTexture123",
                    "slim",
                )],
            ),
            false,
        )
        .await;

    let error = fixture
        .profile_file("http://127.0.0.1:9/texture/".to_string())
        .await
        .expect_err("not ready account should fail");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft account is not ready for profile skin preview",
            "status": "minecraft_account_not_ready",
        })
    );
}

#[tokio::test]
async fn skin_profile_file_requires_sane_texture_url() {
    let fixture = TestFixture::new("profile-file-bad-texture", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin(
                "active",
                "ACTIVE",
                "https://example.com/texture/active?token=secret",
                "slim",
            )],
        ))
        .await;

    let error = fixture
        .profile_file("http://127.0.0.1:9/texture/".to_string())
        .await
        .expect_err("unsane texture should fail");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft profile does not have a usable skin texture",
            "status": "minecraft_profile_skin_missing",
        })
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
        Some("https://textures.minecraft.net/texture/abc".to_string())
    );
    assert_eq!(
        sane_minecraft_texture_url("https://textures.minecraft.net.evil/texture/abc"),
        None
    );
    assert_eq!(
        sane_minecraft_texture_url("http://textures.minecraft.net.evil/texture/abc"),
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
async fn skin_lookup_resolves_username_skin_model_cape_and_head_url() {
    let fixture = TestFixture::new("username-lookup-success", "ConfigUser");
    let texture_prefix = "http://127.0.0.1:9/texture/".to_string();
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("slim".to_string()),
            cape_url: Some(format!("{texture_prefix}usernameCape123")),
        })
        .await;

    let lookup = fixture
        .lookup(
            "  QueryUser  ",
            Some(96),
            profile_endpoint,
            session_endpoint,
            texture_prefix.clone(),
        )
        .await
        .expect("lookup username skin")
        .0;
    let profile_request = profile_requests.recv().await.expect("profile request");
    let session_request = profile_requests.recv().await.expect("session request");

    assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
    assert_eq!(
        session_request.path,
        "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
    );
    assert_eq!(lookup.username, "ResolvedName");
    assert_eq!(lookup.uuid, "0123456789abcdef0123456789abcdef");
    assert_eq!(lookup.source, SAVED_SKIN_USERNAME_SOURCE);
    assert_eq!(lookup.variant, "slim");
    assert_eq!(
        lookup.texture_url,
        format!("{texture_prefix}usernameTexture123")
    );
    assert_eq!(
        lookup.texture_file_url,
        "/api/v1/skin/lookup/file?username=ResolvedName"
    );
    assert_eq!(
        lookup.cape_url,
        Some(format!("{texture_prefix}usernameCape123"))
    );
    assert_eq!(
        lookup.head_url,
        "/api/v1/skin/lookup/head?username=ResolvedName&size=96"
    );
}

#[tokio::test]
async fn skin_lookup_media_reuses_recent_profile_lookup() {
    let fixture = TestFixture::new("username-lookup-profile-cache", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("classic".to_string()),
            cape_url: Some(format!("{texture_prefix}usernameCape123")),
        })
        .await;

    let lookup = fixture
        .lookup(
            "QueryUser",
            Some(96),
            profile_endpoint.clone(),
            session_endpoint.clone(),
            texture_prefix.clone(),
        )
        .await
        .expect("lookup username skin")
        .0;
    let profile_request = profile_requests.recv().await.expect("profile request");
    let session_request = profile_requests.recv().await.expect("session request");

    let file = fixture
        .lookup_file(
            &lookup.username,
            None,
            profile_endpoint.clone(),
            session_endpoint.clone(),
            texture_prefix.clone(),
        )
        .await
        .expect("lookup skin file from cached profile");
    let head = fixture
        .lookup_head(
            &lookup.username,
            Some(32),
            profile_endpoint.clone(),
            session_endpoint.clone(),
            texture_prefix.clone(),
        )
        .await
        .expect("lookup head from cached profile");
    let cape = fixture
        .lookup_cape(
            &lookup.username,
            None,
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("lookup cape from cached profile");

    let file_texture_request = texture_requests.recv().await.expect("skin texture request");
    let cape_texture_request = texture_requests.recv().await.expect("cape texture request");

    assert_eq!(lookup.username, "ResolvedName");
    assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
    assert_eq!(
        session_request.path,
        "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
    );
    assert_eq!(file_texture_request.path, "/texture/usernameTexture123");
    assert_eq!(cape_texture_request.path, "/texture/usernameCape123");
    assert!(matches!(
        profile_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        texture_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let _ = response_bytes(file).await;
    let _ = response_bytes(head).await;
    let _ = response_bytes(cape).await;
}

#[tokio::test]
async fn skin_lookup_file_downloads_normalizes_and_caches_username_skin() {
    let fixture = TestFixture::new("username-lookup-file", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("legacy skin should normalize");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let texture_url = format!("{texture_prefix}usernameTexture123");
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: texture_url.clone(),
            model: Some("classic".to_string()),
            cape_url: None,
        })
        .await;

    let response = fixture
        .lookup_file(
            "QueryUser",
            None,
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("lookup username skin file");
    let profile_request = profile_requests.recv().await.expect("profile request");
    let session_request = profile_requests.recv().await.expect("session request");
    let texture_request = texture_requests.recv().await.expect("texture request");
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let cache_control = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let cache_path =
        profile_skin_file_cache_path(fixture.state.config().paths().skins_dir(), &texture_url);

    assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
    assert_eq!(
        session_request.path,
        "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
    );
    assert_eq!(texture_request.path, "/texture/usernameTexture123");
    assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
    assert_eq!(
        texture_request.user_agent.as_deref(),
        Some(AXIAL_USER_AGENT)
    );
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(
        cache_control.as_deref(),
        Some(PROFILE_SKIN_FILE_CACHE_CONTROL)
    );
    assert_eq!(response_bytes(response).await, normalized.png_bytes);
    assert_eq!(
        tokio::fs::read(cache_path)
            .await
            .expect("read lookup skin cache"),
        normalized.png_bytes
    );
}

#[tokio::test]
async fn skin_lookup_file_cache_hit_avoids_second_texture_request() {
    let fixture = TestFixture::new("username-lookup-file-cache-hit", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png.clone())).await;
    let texture_url = format!("{texture_prefix}usernameTexture123");
    let (profile_endpoint, session_endpoint, _profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url,
            model: Some("classic".to_string()),
            cape_url: None,
        })
        .await;

    let first = fixture
        .lookup_file(
            "QueryUser",
            None,
            profile_endpoint.clone(),
            session_endpoint.clone(),
            texture_prefix.clone(),
        )
        .await
        .expect("first username skin file lookup");
    let texture_request = texture_requests.recv().await.expect("texture request");
    let second = fixture
        .lookup_file(
            "QueryUser",
            None,
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("second username skin file lookup");

    assert_eq!(texture_request.path, "/texture/usernameTexture123");
    assert!(matches!(
        texture_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(response_bytes(first).await, png);
    assert_eq!(response_bytes(second).await, png);
}

#[tokio::test]
async fn skin_lookup_head_downloads_skin_and_returns_png_head() {
    let fixture = TestFixture::new("username-lookup-head", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("classic".to_string()),
            cape_url: None,
        })
        .await;

    let response = fixture
        .lookup_head(
            "QueryUser",
            Some(32),
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("lookup username head");
    let _ = profile_requests.recv().await.expect("profile request");
    let _ = profile_requests.recv().await.expect("session request");
    let texture_request = texture_requests.recv().await.expect("texture request");
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = response_bytes(response).await;
    let decoder = png::Decoder::new(Cursor::new(bytes.as_slice()));
    let reader = decoder.read_info().expect("head png should decode");
    let info = reader.info();

    assert_eq!(texture_request.path, "/texture/usernameTexture123");
    assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(info.width, 32);
    assert_eq!(info.height, 32);
}

#[tokio::test]
async fn skin_lookup_cape_downloads_session_cape_texture() {
    let fixture = TestFixture::new("username-lookup-cape", "ConfigUser");
    let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone())).await;
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("classic".to_string()),
            cape_url: Some(format!("{texture_prefix}usernameCape123")),
        })
        .await;

    let response = fixture
        .lookup_cape(
            "QueryUser",
            None,
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("lookup username cape");
    let profile_request = profile_requests.recv().await.expect("profile request");
    let session_request = profile_requests.recv().await.expect("session request");
    let texture_request = texture_requests.recv().await.expect("cape texture request");
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let cache_control = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
    assert_eq!(
        session_request.path,
        "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
    );
    assert_eq!(texture_request.path, "/texture/usernameCape123");
    assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
    assert_eq!(
        texture_request.user_agent.as_deref(),
        Some(AXIAL_USER_AGENT)
    );
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(
        cache_control.as_deref(),
        Some(PROFILE_CAPE_FILE_CACHE_CONTROL)
    );
    assert_eq!(response_bytes(response).await, cape_png);
}

#[tokio::test]
async fn skin_lookup_cape_cache_hit_avoids_second_texture_request() {
    let fixture = TestFixture::new("username-lookup-cape-cache-hit", "ConfigUser");
    let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone())).await;
    let cape_url = format!("{texture_prefix}usernameCape123");
    let (profile_endpoint, session_endpoint, _profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("classic".to_string()),
            cape_url: Some(cape_url.clone()),
        })
        .await;

    let first = fixture
        .lookup_cape(
            "QueryUser",
            None,
            profile_endpoint.clone(),
            session_endpoint.clone(),
            texture_prefix.clone(),
        )
        .await
        .expect("first username cape lookup");
    let texture_request = texture_requests.recv().await.expect("cape texture request");
    let second = fixture
        .lookup_cape(
            "QueryUser",
            None,
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("second username cape lookup");
    let cache_path =
        profile_cape_file_cache_path(fixture.state.config().paths().skins_dir(), &cape_url);

    assert_eq!(texture_request.path, "/texture/usernameCape123");
    assert!(matches!(
        texture_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(response_bytes(first).await, cape_png);
    assert_eq!(response_bytes(second).await, cape_png);
    assert_eq!(
        tokio::fs::read(cache_path)
            .await
            .expect("read lookup cape cache"),
        cape_png
    );
}

#[tokio::test]
async fn skin_lookup_cape_missing_returns_bounded_conflict() {
    let fixture = TestFixture::new("username-lookup-cape-missing", "ConfigUser");
    let texture_prefix = "http://127.0.0.1:9/texture/".to_string();
    let (profile_endpoint, session_endpoint, _profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("classic".to_string()),
            cape_url: None,
        })
        .await;

    let error = fixture
        .lookup_cape(
            "QueryUser",
            None,
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect_err("missing lookup cape should fail");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft player profile does not have a usable cape texture",
            "status": "minecraft_lookup_cape_missing",
        })
    );
}
