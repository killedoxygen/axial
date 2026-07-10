use super::*;

#[tokio::test]
async fn skin_apply_missing_active_account_returns_bounded_error() {
    let fixture = TestFixture::new("apply-missing-active", "ConfigUser");
    let saved = fixture
        .save_skin("Apply Me", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let error = fixture
        .apply_saved_skin_with_endpoint(&saved.texture_key, "http://127.0.0.1:9/skins")
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
async fn skin_apply_missing_saved_skin_returns_404() {
    let fixture = TestFixture::new("apply-missing-saved", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;

    let error = fixture
        .apply_saved_skin_with_endpoint(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "http://127.0.0.1:9/skins",
        )
        .await
        .expect_err("missing skin should fail");

    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "saved skin not found" })
    );
}

#[tokio::test]
async fn skin_apply_rejects_invalid_texture_key() {
    let fixture = TestFixture::new("apply-invalid-key", "ConfigUser");

    let error = fixture
        .apply_saved_skin_with_endpoint("../not-a-texture-key", "http://127.0.0.1:9/skins")
        .await
        .expect_err("invalid key should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "invalid texture key" })
    );
}

#[tokio::test]
async fn skin_profile_reset_preserves_current_skin_and_clears_local_apply_state() {
    let fixture = TestFixture::new("profile-reset-success", "ConfigUser");
    let external_png = test_slim_skin_png();
    let external_normalized = normalize_skin_png(&external_png).expect("external normalized");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(external_png)).await;
    let external_texture_url = format!("{texture_prefix}externalTexture");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            vec![minecraft_skin(
                "external-skin",
                "ACTIVE",
                &external_texture_url,
                "SLIM",
            )],
            vec![minecraft_cape(
                "external-cape",
                "ACTIVE",
                "https://textures.minecraft.net/texture/externalCape",
            )],
        ))
        .await;
    let applied = fixture
        .save_skin("Applied", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save applied skin")
        .0;
    let queued = fixture
        .save_skin(
            "Queued",
            None,
            test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 83),
        )
        .await
        .expect("save queued skin")
        .0;
    fixture
        .state
        .skins()
        .mark_applied(&applied.texture_key)
        .expect("mark applied skin");
    let _ = fixture
        .queue_saved_skin_apply(&queued.texture_key)
        .await
        .expect("queue pending apply");
    let (reset_endpoint, mut reset_requests) =
        skin_reset_route_test_server(SkinResetServerMode::Success).await;

    let response = fixture
        .reset_profile_skin_with_endpoints(&reset_endpoint, &texture_prefix)
        .await
        .expect("reset profile skin")
        .0;
    let texture_request = texture_requests.recv().await.expect("texture request");
    let reset_request = reset_requests.recv().await.expect("reset request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let account = fixture
        .state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .expect("active minecraft account")
        .account;
    let external_texture_key = texture_key(&external_normalized.png_bytes);
    let preserved = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == external_texture_key)
        .expect("external profile skin preserved");

    assert_eq!(response.status, "reset");
    assert!(response.profile_updated);
    assert_eq!(texture_request.path, "/texture/externalTexture");
    assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
    assert_eq!(
        texture_request.user_agent.as_deref(),
        Some(AXIAL_USER_AGENT)
    );
    assert_eq!(reset_request.method, "DELETE");
    assert_eq!(reset_request.path, "/minecraft/profile/skins/active");
    assert_eq!(
        reset_request.authorization.as_deref(),
        Some("Bearer minecraft-access-token")
    );
    assert_eq!(reset_request.accept.as_deref(), Some("application/json"));
    assert_eq!(reset_request.user_agent.as_deref(), Some(AXIAL_USER_AGENT));
    assert_eq!(listed.pending_apply_texture_key, None);
    assert!(listed.skins.iter().all(|skin| skin.applied_at.is_none()));
    assert_eq!(preserved.name, "MinecraftName profile skin");
    assert_eq!(preserved.source, SAVED_SKIN_PROFILE_SOURCE);
    assert_eq!(preserved.variant, "slim");
    assert_eq!(preserved.cape_id.as_deref(), Some("external-cape"));
    assert_eq!(account.profile.name, "ResetProfileName");
    assert!(account.profile.skins.is_empty());
}

#[tokio::test]
async fn skin_profile_reset_does_not_call_upstream_when_preservation_fails() {
    let fixture = TestFixture::new("profile-reset-preserve-fails", "ConfigUser");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
    let external_texture_url = format!("{texture_prefix}externalTexture");
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin(
                "external-skin",
                "ACTIVE",
                &external_texture_url,
                "classic",
            )],
        ))
        .await;
    let (reset_endpoint, mut reset_requests) =
        skin_reset_route_test_server(SkinResetServerMode::Success).await;

    let error = fixture
        .reset_profile_skin_with_endpoints(&reset_endpoint, &texture_prefix)
        .await
        .expect_err("preservation failure should stop reset");
    let texture_request = texture_requests.recv().await.expect("texture request");

    assert_eq!(texture_request.path, "/texture/externalTexture");
    assert!(matches!(
        reset_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Current Minecraft profile skin is too large to preserve before changing it",
            "status": "minecraft_profile_skin_preserve_too_large",
        })
    );
}

#[tokio::test]
async fn skin_profile_reset_upstream_429_maps_to_bounded_rate_limit() {
    let fixture = TestFixture::new("profile-reset-rate-limit", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let (reset_endpoint, mut reset_requests) =
        skin_reset_route_test_server(SkinResetServerMode::RateLimited).await;

    let error = fixture
        .reset_profile_skin_with_endpoints(&reset_endpoint, "http://127.0.0.1:9/texture/")
        .await
        .expect_err("rate limited reset should fail");
    let request = reset_requests.recv().await.expect("reset request");

    assert_eq!(request.method, "DELETE");
    assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft skin reset is rate limited. Try again later.",
            "status": "minecraft_skin_reset_rate_limited",
        })
    );
}

#[tokio::test]
async fn skin_cape_reset_preserves_current_skin_and_clears_local_apply_state() {
    let fixture = TestFixture::new("cape-reset-success", "ConfigUser");
    let external_png = test_slim_skin_png();
    let external_normalized = normalize_skin_png(&external_png).expect("external normalized");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(external_png)).await;
    let external_texture_url = format!("{texture_prefix}externalTexture");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            vec![minecraft_skin(
                "external-skin",
                "ACTIVE",
                &external_texture_url,
                "SLIM",
            )],
            vec![minecraft_cape(
                "external-cape",
                "ACTIVE",
                "https://textures.minecraft.net/texture/externalCape",
            )],
        ))
        .await;
    let applied = fixture
        .save_skin("Applied", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save applied skin")
        .0;
    let queued = fixture
        .save_skin(
            "Queued",
            None,
            test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 97),
        )
        .await
        .expect("save queued skin")
        .0;
    fixture
        .state
        .skins()
        .mark_applied(&applied.texture_key)
        .expect("mark applied skin");
    let _ = fixture
        .queue_saved_skin_apply(&queued.texture_key)
        .await
        .expect("queue pending apply");
    let (cape_endpoint, mut cape_requests) =
        cape_sync_route_test_server(CapeSyncServerMode::Success).await;

    let response = fixture
        .reset_profile_cape_with_endpoints(&cape_endpoint, &texture_prefix)
        .await
        .expect("reset profile cape")
        .0;
    let texture_request = texture_requests.recv().await.expect("texture request");
    let cape_request = cape_requests.recv().await.expect("cape reset request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let account = fixture
        .state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .expect("active minecraft account")
        .account;
    let external_texture_key = texture_key(&external_normalized.png_bytes);
    let preserved = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == external_texture_key)
        .expect("external profile skin preserved");

    assert_eq!(response.status, "reset");
    assert!(response.profile_updated);
    assert_eq!(texture_request.path, "/texture/externalTexture");
    assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
    assert_eq!(
        texture_request.user_agent.as_deref(),
        Some(AXIAL_USER_AGENT)
    );
    assert_eq!(cape_request.method, "DELETE");
    assert_eq!(cape_request.path, "/minecraft/profile/capes/active");
    assert_eq!(
        cape_request.authorization.as_deref(),
        Some("Bearer minecraft-access-token")
    );
    assert_eq!(cape_request.accept.as_deref(), Some("application/json"));
    assert_eq!(cape_request.user_agent.as_deref(), Some(AXIAL_USER_AGENT));
    assert_eq!(listed.pending_apply_texture_key, None);
    assert!(listed.skins.iter().all(|skin| skin.applied_at.is_none()));
    assert_eq!(preserved.name, "MinecraftName profile skin");
    assert_eq!(preserved.source, SAVED_SKIN_PROFILE_SOURCE);
    assert_eq!(preserved.variant, "slim");
    assert_eq!(preserved.cape_id.as_deref(), Some("external-cape"));
    assert_eq!(account.profile.capes[0].state, "INACTIVE");
}

#[tokio::test]
async fn skin_cape_reset_does_not_call_upstream_when_preservation_fails() {
    let fixture = TestFixture::new("cape-reset-preserve-fails", "ConfigUser");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
    let external_texture_url = format!("{texture_prefix}externalTexture");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            vec![minecraft_skin(
                "external-skin",
                "ACTIVE",
                &external_texture_url,
                "classic",
            )],
            vec![minecraft_cape(
                "external-cape",
                "ACTIVE",
                "https://textures.minecraft.net/texture/externalCape",
            )],
        ))
        .await;
    let (cape_endpoint, mut cape_requests) =
        cape_sync_route_test_server(CapeSyncServerMode::Success).await;

    let error = fixture
        .reset_profile_cape_with_endpoints(&cape_endpoint, &texture_prefix)
        .await
        .expect_err("preservation failure should stop cape reset");
    let texture_request = texture_requests.recv().await.expect("texture request");

    assert_eq!(texture_request.path, "/texture/externalTexture");
    assert!(matches!(
        cape_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Current Minecraft profile skin is too large to preserve before changing it",
            "status": "minecraft_profile_skin_preserve_too_large",
        })
    );
}

#[tokio::test]
async fn skin_cape_reset_upstream_429_maps_to_bounded_rate_limit() {
    let fixture = TestFixture::new("cape-reset-rate-limit", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            Vec::new(),
            vec![minecraft_cape(
                "external-cape",
                "ACTIVE",
                "https://textures.minecraft.net/texture/externalCape",
            )],
        ))
        .await;
    let (cape_endpoint, mut cape_requests) =
        cape_sync_route_test_server(CapeSyncServerMode::RateLimited).await;

    let error = fixture
        .reset_profile_cape_with_endpoints(&cape_endpoint, "http://127.0.0.1:9/texture/")
        .await
        .expect_err("rate limited cape reset should fail");
    let request = cape_requests.recv().await.expect("cape reset request");

    assert_eq!(request.method, "DELETE");
    assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft cape change is rate limited. Try again later.",
            "status": "minecraft_cape_rate_limited",
        })
    );
}

#[tokio::test]
async fn skin_apply_upstream_success_uploads_saved_skin_and_updates_profile() {
    let fixture = TestFixture::new("apply-upstream-success", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("OldMinecraftName", Vec::new()))
        .await;
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let saved = fixture
        .save_skin("Slim Skin", Some("slim".to_string()), png.clone())
        .await
        .expect("save skin")
        .0;
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (endpoint, mut requests) = skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let response = fixture
        .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
        .await
        .expect("apply skin")
        .0;
    let request = requests.recv().await.expect("skin upload request");
    let account = fixture
        .state
        .auth_logins()
        .active_minecraft_account()
        .await
        .expect("active minecraft account");

    assert_eq!(response.status, "applied");
    assert_eq!(response.texture_key, saved.texture_key);
    assert!(response.profile_updated);
    assert_eq!(request.path, "/minecraft/profile/skins");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer minecraft-access-token")
    );
    assert_eq!(request.accept.as_deref(), Some("application/json"));
    assert_eq!(request.user_agent.as_deref(), Some(AXIAL_USER_AGENT));
    assert!(
        request
            .content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("multipart/form-data; boundary="))
    );
    assert!(body_contains(&request.body, b"name=\"variant\""));
    assert!(body_contains(&request.body, b"slim"));
    assert!(body_contains(
        &request.body,
        b"name=\"file\"; filename=\"skin.png\""
    ));
    assert!(body_contains(&request.body, &normalized.png_bytes));
    assert_eq!(account.profile.name, "UpdatedProfileName");
    assert_eq!(account.profile.skins[0].variant, "SLIM");
}

#[tokio::test]
async fn skin_apply_success_marks_saved_skin_applied_and_clears_prior_marker() {
    let fixture = TestFixture::new("apply-marks-active", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let prior = fixture
        .save_skin("Prior", None, test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT))
        .await
        .expect("save prior skin")
        .0;
    let next = fixture
        .save_skin(
            "Next",
            Some("slim".to_string()),
            test_skin_png(SKIN_WIDTH, SKIN_HEIGHT),
        )
        .await
        .expect("save next skin")
        .0;
    fixture
        .state
        .skins()
        .mark_applied(&prior.texture_key)
        .expect("mark prior skin applied");
    let (endpoint, mut requests) = skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let _ = fixture
        .apply_saved_skin_with_endpoint(&next.texture_key, &endpoint)
        .await
        .expect("apply next skin");
    let _ = requests.recv().await.expect("skin upload request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let prior_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == prior.texture_key)
        .expect("prior skin listed");
    let next_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == next.texture_key)
        .expect("next skin listed");

    assert_eq!(prior_after.applied_at, None);
    assert!(next_after.applied_at.is_some());
}

#[tokio::test]
async fn skin_apply_defer_queues_until_flush() {
    let fixture = TestFixture::new("apply-defer-flush", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    let (endpoint, mut requests) = skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let queued = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue skin apply")
        .0;
    let listed_before_flush = fixture.saved_skins().await.expect("saved skins").0;
    let saved_before_flush = listed_before_flush
        .skins
        .iter()
        .find(|skin| skin.texture_key == saved.texture_key)
        .expect("saved skin listed");

    assert_eq!(queued.status, "queued");
    assert_eq!(queued.texture_key, saved.texture_key);
    assert!(!queued.profile_updated);
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(
        listed_before_flush.pending_apply_texture_key.as_deref(),
        Some(saved.texture_key.as_str())
    );
    assert_eq!(saved_before_flush.applied_at, None);

    let flushed = fixture
        .flush_saved_skin_applies_with_endpoints(
            &endpoint,
            "http://127.0.0.1:9/capes",
            "http://127.0.0.1:9/texture/",
        )
        .await
        .expect("flush pending skin apply")
        .0;
    let _ = requests.recv().await.expect("skin upload request");
    let listed_after_flush = fixture.saved_skins().await.expect("saved skins").0;
    let saved_after_flush = listed_after_flush
        .skins
        .iter()
        .find(|skin| skin.texture_key == saved.texture_key)
        .expect("saved skin listed");

    assert_eq!(flushed.status, "flushed");
    assert_eq!(flushed.applied, 1);
    assert_eq!(listed_after_flush.pending_apply_texture_key, None);
    assert!(saved_after_flush.applied_at.is_some());
}

#[tokio::test]
async fn skin_apply_shutdown_flushes_active_pending_change() {
    let fixture = TestFixture::new("apply-shutdown-flush", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin(
            "Shutdown Queued",
            None,
            test_skin_png(SKIN_WIDTH, SKIN_HEIGHT),
        )
        .await
        .expect("save skin")
        .0;
    let (endpoint, mut requests) = skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let _ = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue skin apply");
    let flushed = fixture
        .flush_saved_skin_applies_with_endpoints(
            &endpoint,
            "http://127.0.0.1:9/capes",
            "http://127.0.0.1:9/texture/",
        )
        .await
        .expect("shutdown flush pending skin apply")
        .0;
    let _ = requests.recv().await.expect("skin upload request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let saved_after_flush = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == saved.texture_key)
        .expect("saved skin listed");

    assert_eq!(flushed.status, "flushed");
    assert_eq!(flushed.applied, 1);
    assert_eq!(listed.pending_apply_texture_key, None);
    assert!(saved_after_flush.applied_at.is_some());
}

#[tokio::test]
async fn skin_apply_defer_clear_removes_pending_for_active_account() {
    let fixture = TestFixture::new("apply-defer-clear", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let _ = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue skin apply");
    let listed_before_clear = fixture.saved_skins().await.expect("saved skins").0;
    let cleared = fixture
        .clear_pending_saved_skin_apply()
        .await
        .expect("clear pending skin apply")
        .0;
    let listed_after_clear = fixture.saved_skins().await.expect("saved skins").0;

    assert_eq!(
        listed_before_clear.pending_apply_texture_key.as_deref(),
        Some(saved.texture_key.as_str())
    );
    assert_eq!(cleared.status, "cleared");
    assert!(cleared.cleared);
    assert_eq!(listed_after_clear.pending_apply_texture_key, None);
}

#[tokio::test]
async fn skin_apply_clear_for_login_id_removes_pending_apply() {
    let fixture = TestFixture::new("apply-clear-login-id", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let _ = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue skin apply");
    let login_id = fixture
        .state
        .auth_logins()
        .active_minecraft_account()
        .await
        .expect("active minecraft account")
        .login_id;

    assert!(clear_pending_saved_skin_apply_for_login_id(&login_id).await);
    assert!(!clear_pending_saved_skin_apply_for_login_id(&login_id).await);
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    assert_eq!(listed.pending_apply_texture_key, None);
}

#[tokio::test]
async fn skin_apply_defer_keeps_latest_for_same_account() {
    let fixture = TestFixture::new("apply-defer-latest-wins", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let prior_png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
    let next_png = test_slim_skin_png();
    let next_normalized = normalize_skin_png(&next_png).expect("next normalized");
    let prior = fixture
        .save_skin("Prior", None, prior_png)
        .await
        .expect("save prior")
        .0;
    let next = fixture
        .save_skin("Next", Some("slim".to_string()), next_png)
        .await
        .expect("save next")
        .0;
    let (endpoint, mut requests) = skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let _ = fixture
        .queue_saved_skin_apply(&prior.texture_key)
        .await
        .expect("queue prior");
    let _ = fixture
        .queue_saved_skin_apply(&next.texture_key)
        .await
        .expect("queue next");
    let flushed = fixture
        .flush_saved_skin_applies_with_endpoints(
            &endpoint,
            "http://127.0.0.1:9/capes",
            "http://127.0.0.1:9/texture/",
        )
        .await
        .expect("flush pending skin apply")
        .0;
    let request = requests.recv().await.expect("skin upload request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let prior_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == prior.texture_key)
        .expect("prior skin listed");
    let next_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == next.texture_key)
        .expect("next skin listed");

    assert_eq!(flushed.applied, 1);
    assert!(body_contains(&request.body, &next_normalized.png_bytes));
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(prior_after.applied_at, None);
    assert!(next_after.applied_at.is_some());
}

#[tokio::test]
async fn skin_apply_defer_flushes_against_queued_login_after_account_switch() {
    let fixture = TestFixture::new("apply-defer-original-login", "ConfigUser");
    let first_account = fixture
        .add_minecraft_account_with_tokens(
            test_profile("FirstPlayer", Vec::new()),
            "first-msa-access-token",
            "first-minecraft-access-token",
        )
        .await;
    let saved = fixture
        .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    let (endpoint, mut requests) = skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let _ = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue skin apply");
    let second_account = fixture
        .add_minecraft_account_with_tokens(
            test_profile("SecondPlayer", Vec::new()),
            "second-msa-access-token",
            "second-minecraft-access-token",
        )
        .await;

    assert_ne!(first_account.login_id, second_account.login_id);
    assert_eq!(
        fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .expect("second account active")
            .account
            .login_id,
        second_account.login_id
    );

    let applied = flush_pending_saved_skin_applies_with_clients(
        &fixture.state,
        PendingSkinApplyFilter::Generation {
            login_id: first_account.login_id.clone(),
            generation: 1,
        },
        MinecraftSkinUploadClient::with_endpoint(endpoint),
        MinecraftCapeSyncClient::with_endpoint("http://127.0.0.1:9/capes".to_string()),
        MinecraftSkinTextureClient::with_allowed_prefix("http://127.0.0.1:9/texture/".to_string()),
    )
    .await
    .expect("flush queued skin apply");
    let request = requests.recv().await.expect("skin upload request");

    assert_eq!(applied, 1);
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer first-minecraft-access-token")
    );
    assert_eq!(
        fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .expect("second account remains active")
            .account
            .login_id,
        second_account.login_id
    );
}

#[tokio::test]
async fn skin_apply_flush_requeues_failed_pending_change() {
    let fixture = TestFixture::new("apply-defer-requeues-failure", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin("Retry", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    let (rejected_endpoint, mut rejected_requests) =
        skin_apply_route_test_server(SkinApplyServerMode::Rejected).await;
    let (success_endpoint, mut success_requests) =
        skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let _ = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue skin apply");
    let error = fixture
        .flush_saved_skin_applies_with_endpoints(
            &rejected_endpoint,
            "http://127.0.0.1:9/capes",
            "http://127.0.0.1:9/texture/",
        )
        .await
        .expect_err("rejected flush should fail");
    let _ = rejected_requests
        .recv()
        .await
        .expect("rejected skin upload request");
    let listed_after_error = fixture.saved_skins().await.expect("saved skins").0;
    let saved_after_error = listed_after_error
        .skins
        .iter()
        .find(|skin| skin.texture_key == saved.texture_key)
        .expect("saved skin listed");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(saved_after_error.applied_at, None);

    let flushed = fixture
        .flush_saved_skin_applies_with_endpoints(
            &success_endpoint,
            "http://127.0.0.1:9/capes",
            "http://127.0.0.1:9/texture/",
        )
        .await
        .expect("retry pending skin apply")
        .0;
    let _ = success_requests
        .recv()
        .await
        .expect("success skin upload request");
    let listed_after_retry = fixture.saved_skins().await.expect("saved skins").0;
    let saved_after_retry = listed_after_retry
        .skins
        .iter()
        .find(|skin| skin.texture_key == saved.texture_key)
        .expect("saved skin listed");

    assert_eq!(flushed.applied, 1);
    assert!(saved_after_retry.applied_at.is_some());
}

#[tokio::test]
async fn skin_apply_success_syncs_selected_cape() {
    let fixture = TestFixture::new("apply-syncs-cape", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            Vec::new(),
            vec![minecraft_cape(
                "cape-id",
                "INACTIVE",
                "https://textures.minecraft.net/texture/capeTexture",
            )],
        ))
        .await;
    let saved = fixture
        .save_skin("Cape Skin", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    let saved = fixture
        .update_saved_skin(
            &saved.texture_key,
            serde_json::json!({ "cape_id": "cape-id" }),
        )
        .await
        .expect("select cape")
        .0;
    let (skin_endpoint, mut skin_requests) =
        skin_apply_route_test_server(SkinApplyServerMode::SuccessWithCapeAvailable).await;
    let (cape_endpoint, mut cape_requests) =
        cape_sync_route_test_server(CapeSyncServerMode::Success).await;

    let response = fixture
        .apply_saved_skin_with_endpoints(&saved.texture_key, &skin_endpoint, &cape_endpoint)
        .await
        .expect("apply saved skin with cape")
        .0;
    let _ = skin_requests.recv().await.expect("skin upload request");
    let cape_request = cape_requests.recv().await.expect("cape sync request");
    let account = fixture
        .state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .expect("active minecraft account")
        .account;
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let saved_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == saved.texture_key)
        .expect("saved skin listed");

    assert!(response.profile_updated);
    assert!(saved_after.applied_at.is_some());
    assert_eq!(cape_request.method, "PUT");
    assert_eq!(cape_request.path, "/minecraft/profile/capes/active");
    assert_eq!(
        cape_request.authorization.as_deref(),
        Some("Bearer minecraft-access-token")
    );
    assert_eq!(cape_request.accept.as_deref(), Some("application/json"));
    assert_eq!(cape_request.user_agent.as_deref(), Some(AXIAL_USER_AGENT));
    assert_eq!(
        cape_request.content_type.as_deref(),
        Some("application/json")
    );
    assert!(body_contains(&cape_request.body, br#""capeId":"cape-id""#));
    assert_eq!(account.profile.capes[0].state, "ACTIVE");
}

#[tokio::test]
async fn skin_apply_preserves_external_profile_skin_before_upload() {
    let fixture = TestFixture::new("apply-preserves-external", "ConfigUser");
    let external_png = test_slim_skin_png();
    let external_normalized = normalize_skin_png(&external_png).expect("external normalized");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(external_png)).await;
    let external_texture_url = format!("{texture_prefix}externalTexture");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            vec![minecraft_skin(
                "external-skin",
                "ACTIVE",
                &external_texture_url,
                "SLIM",
            )],
            vec![minecraft_cape(
                "external-cape",
                "ACTIVE",
                "https://textures.minecraft.net/texture/externalCape",
            )],
        ))
        .await;
    let target = fixture
        .save_skin("Target", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save target skin")
        .0;
    let (skin_endpoint, mut skin_requests) =
        skin_apply_route_test_server(SkinApplyServerMode::Success).await;
    let (cape_endpoint, mut cape_requests) =
        cape_sync_route_test_server(CapeSyncServerMode::Success).await;

    let response = fixture
        .apply_saved_skin_with_all_endpoints(
            &target.texture_key,
            &skin_endpoint,
            &cape_endpoint,
            &texture_prefix,
        )
        .await
        .expect("apply skin")
        .0;
    let texture_request = texture_requests.recv().await.expect("texture request");
    let _ = skin_requests.recv().await.expect("skin upload request");
    let cape_request = cape_requests.recv().await.expect("cape sync request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let external_texture_key = texture_key(&external_normalized.png_bytes);
    let preserved = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == external_texture_key)
        .expect("external profile skin preserved");

    assert_eq!(response.status, "applied");
    assert_eq!(texture_request.path, "/texture/externalTexture");
    assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
    assert_eq!(
        texture_request.user_agent.as_deref(),
        Some(AXIAL_USER_AGENT)
    );
    assert_eq!(cape_request.method, "DELETE");
    assert_eq!(preserved.name, "MinecraftName profile skin");
    assert_eq!(preserved.source, SAVED_SKIN_PROFILE_SOURCE);
    assert_eq!(preserved.variant, "slim");
    assert_eq!(preserved.cape_id.as_deref(), Some("external-cape"));
    assert_eq!(preserved.applied_at, None);
}

#[tokio::test]
async fn skin_apply_does_not_upload_when_external_preservation_fails() {
    let fixture = TestFixture::new("apply-preserve-fails-before-upload", "ConfigUser");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
    let external_texture_url = format!("{texture_prefix}externalTexture");
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin(
                "external-skin",
                "ACTIVE",
                &external_texture_url,
                "CLASSIC",
            )],
        ))
        .await;
    let target = fixture
        .save_skin("Target", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save target skin")
        .0;
    let (skin_endpoint, mut skin_requests) =
        skin_apply_route_test_server(SkinApplyServerMode::Success).await;

    let error = fixture
        .apply_saved_skin_with_all_endpoints(
            &target.texture_key,
            &skin_endpoint,
            "http://127.0.0.1:9/capes",
            &texture_prefix,
        )
        .await
        .expect_err("preservation failure should stop apply");
    let texture_request = texture_requests.recv().await.expect("texture request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let target_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == target.texture_key)
        .expect("target skin listed");

    assert_eq!(texture_request.path, "/texture/externalTexture");
    assert!(matches!(
        skin_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(listed.skins.len(), 1);
    assert_eq!(target_after.applied_at, None);
    assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Current Minecraft profile skin is too large to preserve before changing it",
            "status": "minecraft_profile_skin_preserve_too_large",
        })
    );
}

#[tokio::test]
async fn skin_apply_upstream_failure_does_not_mark_saved_skin_applied() {
    let fixture = TestFixture::new("apply-failure-keeps-marker", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let prior = fixture
        .save_skin("Prior", None, test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT))
        .await
        .expect("save prior skin")
        .0;
    let rejected = fixture
        .save_skin("Rejected", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save rejected skin")
        .0;
    let prior_applied_at = fixture
        .state
        .skins()
        .mark_applied(&prior.texture_key)
        .expect("mark prior skin applied")
        .expect("prior skin exists");
    let (endpoint, mut requests) =
        skin_apply_route_test_server(SkinApplyServerMode::Rejected).await;

    let _ = fixture
        .apply_saved_skin_with_endpoint(&rejected.texture_key, &endpoint)
        .await
        .expect_err("rejected upload should fail");
    let _ = requests.recv().await.expect("skin upload request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let prior_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == prior.texture_key)
        .expect("prior skin listed");
    let rejected_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == rejected.texture_key)
        .expect("rejected skin listed");

    assert_eq!(
        prior_after.applied_at.as_deref(),
        Some(prior_applied_at.as_str())
    );
    assert_eq!(rejected_after.applied_at, None);
}

#[tokio::test]
async fn skin_apply_oversized_success_response_is_bounded() {
    let fixture = TestFixture::new("apply-oversized-success-response", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin(
            "Oversized Response",
            None,
            test_skin_png(SKIN_WIDTH, SKIN_HEIGHT),
        )
        .await
        .expect("save skin")
        .0;
    let (endpoint, mut requests) =
        skin_apply_route_test_server(SkinApplyServerMode::OversizedSuccess).await;

    let error = fixture
        .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
        .await
        .expect_err("oversized upload response should fail");
    let _ = requests.recv().await.expect("skin upload request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let saved_after = listed
        .skins
        .iter()
        .find(|skin| skin.texture_key == saved.texture_key)
        .expect("saved skin listed");

    assert_eq!(error.0, StatusCode::BAD_GATEWAY);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft skin upload response is too large",
            "status": "minecraft_skin_response_too_large",
        })
    );
    assert_eq!(saved_after.applied_at, None);
}

#[tokio::test]
async fn skin_apply_upstream_429_maps_to_bounded_rate_limit() {
    let fixture = TestFixture::new("apply-upstream-rate-limit", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin("Rate Limited", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    let (endpoint, mut requests) =
        skin_apply_route_test_server(SkinApplyServerMode::RateLimited).await;

    let error = fixture
        .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
        .await
        .expect_err("rate limited upload should fail");
    let _ = requests.recv().await.expect("skin upload request");

    assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft skin upload is rate limited. Try again later.",
            "status": "minecraft_skin_rate_limited",
        })
    );
}

#[tokio::test]
async fn skin_apply_upstream_rejected_error_is_bounded() {
    let fixture = TestFixture::new("apply-upstream-rejected", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin("Rejected", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    let (endpoint, mut requests) =
        skin_apply_route_test_server(SkinApplyServerMode::Rejected).await;

    let error = fixture
        .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
        .await
        .expect_err("rejected upload should fail");
    let _ = requests.recv().await.expect("skin upload request");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft rejected the saved skin",
            "status": "minecraft_skin_rejected",
        })
    );
}

#[tokio::test]
async fn skin_apply_upstream_unavailable_error_is_bounded() {
    let fixture = TestFixture::new("apply-upstream-unavailable", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin("Unavailable", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let error = fixture
        .apply_saved_skin_with_endpoint(&saved.texture_key, "http://127.0.0.1:9/skins")
        .await
        .expect_err("unavailable upload should fail");

    assert_eq!(error.0, StatusCode::BAD_GATEWAY);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft skin upload is unavailable. Try again later.",
            "status": "minecraft_skin_unavailable",
        })
    );
}
