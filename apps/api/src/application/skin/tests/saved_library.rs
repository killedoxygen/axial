use super::*;

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
    assert!(
        response
            .normalized_data_url
            .starts_with("data:image/png;base64,")
    );
}

#[tokio::test]
async fn skin_normalize_64x64_png_suggests_slim_when_arm_region_is_transparent() {
    let png = test_slim_skin_png();
    let normalized = normalize_skin_png(&png).expect("normalized slim skin");
    let decoded = decode_skin_png(&normalized.png_bytes).expect("normalized slim pixels");

    let response = normalize_skin_body(png)
        .await
        .expect("normalize response")
        .0;

    assert_eq!(response.variant_suggestion, "slim");
    assert_eq!(response.original_width, SKIN_WIDTH);
    assert_eq!(response.original_height, SKIN_HEIGHT);
    assert_eq!(skin_rgba_pixel(&decoded.rgba, 54, 20)[3], 0);
    assert_eq!(skin_rgba_pixel(&decoded.rgba, 8, 8)[3], 255);
    assert_texture_key(&response.texture_key);
}

#[tokio::test]
async fn skin_normalize_64x32_png_normalizes_to_64x64() {
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let expected = normalize_skin_png(&png).expect("expected normalized skin");

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
    assert_eq!(response.variant_suggestion, "classic");
    assert_eq!(response.normalized_width, SKIN_WIDTH);
    assert_eq!(response.normalized_height, SKIN_HEIGHT);
    assert_eq!(response.texture_key, repeated.texture_key);
    assert_eq!(response.normalized_byte_size, repeated.normalized_byte_size);
    assert_eq!(response.normalized_byte_size, expected.png_bytes.len());
    let decoded = decode_skin_png(&expected.png_bytes).expect("normalized legacy pixels");
    assert_eq!(
        skin_rgba_pixel(&decoded.rgba, 20, 52),
        skin_rgba_pixel(&decoded.rgba, 4, 20)
    );
    assert_eq!(
        skin_rgba_pixel(&decoded.rgba, 36, 52),
        skin_rgba_pixel(&decoded.rgba, 44, 20)
    );
    assert_eq!(
        response.normalized_data_url,
        format!(
            "data:image/png;base64,{}",
            BASE64_STANDARD.encode(expected.png_bytes)
        )
    );
    assert_texture_key(&response.texture_key);
}

#[tokio::test]
async fn skin_normalize_legacy_base_pixels_are_opaque() {
    let mut rgba = test_skin_rgba(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    set_skin_rgba_alpha(&mut rgba, 8, 8, 0);
    set_skin_rgba_alpha(&mut rgba, 4, 31, 0);
    set_skin_rgba_alpha(&mut rgba, 44, 31, 0);
    let bad_cached = encode_test_png(SKIN_WIDTH, SKIN_HEIGHT, &normalize_legacy_skin_rgba(&rgba));

    let normalized = normalize_skin_png(&encode_test_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT, &rgba))
        .expect("transparent legacy base pixels should normalize");
    let decoded = decode_skin_png(&normalized.png_bytes).expect("normalized legacy pixels");

    assert!(!is_valid_normalized_skin_cache_png(&bad_cached));
    assert_eq!(skin_rgba_pixel(&decoded.rgba, 8, 8), [24, 40, 16, 255]);
    assert_eq!(skin_rgba_pixel(&decoded.rgba, 4, 31), [12, 155, 35, 255]);
    assert_eq!(skin_rgba_pixel(&decoded.rgba, 20, 63), [12, 155, 35, 255]);
    assert_eq!(skin_rgba_pixel(&decoded.rgba, 36, 63), [132, 155, 75, 255]);
    assert!(is_valid_normalized_skin_cache_png(&normalized.png_bytes));
}

#[tokio::test]
async fn skin_normalize_fully_opaque_legacy_head_overlay_is_cleared() {
    let mut rgba = test_skin_rgba(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    fill_skin_rgba_region(&mut rgba, 32, 0, 32, 16, [12, 34, 56, 255]);

    let normalized = normalize_skin_png(&encode_test_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT, &rgba))
        .expect("fully opaque legacy overlay should normalize");
    let decoded = decode_skin_png(&normalized.png_bytes).expect("normalized legacy pixels");
    let mut stale_cached = normalize_legacy_skin_rgba(&rgba);
    fill_skin_rgba_region(&mut stale_cached, 32, 0, 32, 16, [12, 34, 56, 255]);

    assert_eq!(skin_rgba_pixel(&decoded.rgba, 8, 8), [24, 40, 16, 255]);
    assert_eq!(skin_rgba_pixel(&decoded.rgba, 40, 8), [0, 0, 0, 0]);
    assert!(!is_valid_normalized_skin_cache_png(&encode_test_png(
        SKIN_WIDTH,
        SKIN_HEIGHT,
        &stale_cached
    )));
    assert!(is_valid_normalized_skin_cache_png(&normalized.png_bytes));
}

#[tokio::test]
async fn skin_normalize_padded_legacy_64x64_repairs_left_limbs() {
    let legacy = test_skin_rgba(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let mut padded = vec![0; (SKIN_WIDTH * SKIN_HEIGHT * 4) as usize];
    let row_len = (SKIN_WIDTH * 4) as usize;
    for row in 0..LEGACY_SKIN_HEIGHT as usize {
        let offset = row * row_len;
        padded[offset..offset + row_len].copy_from_slice(&legacy[offset..offset + row_len]);
    }

    let normalized = normalize_skin_png(&encode_test_png(SKIN_WIDTH, SKIN_HEIGHT, &padded))
        .expect("padded legacy skin should normalize");
    let decoded = decode_skin_png(&normalized.png_bytes).expect("normalized padded legacy pixels");

    assert_eq!(normalized.original_width, SKIN_WIDTH);
    assert_eq!(normalized.original_height, SKIN_HEIGHT);
    assert_eq!(normalized.variant_suggestion, "classic");
    assert_eq!(
        skin_rgba_pixel(&decoded.rgba, 24, 52),
        skin_rgba_pixel(&decoded.rgba, 0, 20)
    );
    assert_eq!(
        skin_rgba_pixel(&decoded.rgba, 40, 52),
        skin_rgba_pixel(&decoded.rgba, 40, 20)
    );
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

#[test]
fn skin_png_validator_rejects_signature_bearing_malformed_png() {
    let mut bytes = PNG_SIGNATURE.to_vec();
    bytes.extend_from_slice(b"malformed");

    assert_eq!(
        validate_skin_png(&bytes),
        Err(SkinPngValidationError::InvalidPng)
    );
}

#[test]
fn skin_png_validator_rejects_invalid_dimensions() {
    assert_eq!(
        validate_skin_png(&test_skin_png(32, 32)),
        Err(SkinPngValidationError::InvalidDimensions)
    );
}

#[test]
fn skin_png_validator_rejects_bytes_after_iend() {
    let mut bytes = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
    bytes.extend_from_slice(b"trailing data");

    assert_eq!(
        validate_skin_png(&bytes),
        Err(SkinPngValidationError::InvalidPng)
    );
}

#[test]
fn skin_png_validator_ignores_compressed_text_and_profile_chunks() {
    let bytes = test_skin_png_with_compressed_ancillary_chunks();

    assert!(bytes.len() <= SKIN_PNG_MAX_BYTES);
    assert_eq!(validate_skin_png(&bytes), Ok(()));
}

#[test]
fn skin_png_validator_enforces_the_decoder_allocation_budget() {
    let bytes = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);

    assert_eq!(
        validate_skin_png_with_budget(&bytes, 0),
        Err(SkinPngValidationError::InvalidPng)
    );
}

#[test]
fn skin_png_validator_accepts_the_maximum_bounded_input() {
    let bytes = test_skin_png_with_exact_len(SKIN_PNG_MAX_BYTES);

    assert_eq!(bytes.len(), SKIN_PNG_MAX_BYTES);
    assert_eq!(validate_skin_png(&bytes), Ok(()));

    let mut oversized = bytes;
    oversized.push(0);
    assert_eq!(
        validate_skin_png(&oversized),
        Err(SkinPngValidationError::TooLarge)
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

#[tokio::test]
async fn skin_saved_list_initially_empty() {
    let fixture = TestFixture::new("saved-list-empty", "ConfigUser");

    let response = fixture.saved_skins().await.expect("saved skins").0;

    assert!(response.skins.is_empty());
    assert_eq!(response.pending_apply_texture_key, None);
}

#[tokio::test]
async fn skin_saved_save_lists_metadata_without_bytes() {
    let fixture = TestFixture::new("saved-save-list", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);

    let saved = fixture
        .save_skin("  My Skin  ", Some("slim".to_string()), png.clone())
        .await
        .expect("save skin")
        .0;
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let file = fixture
        .saved_skin_file(&saved.texture_key)
        .await
        .expect("saved skin file");
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
    let file_bytes = response_bytes(file).await;
    let normalized = normalize_skin_png(&png).expect("normalized skin");

    assert_eq!(listed.skins, vec![saved.clone()]);
    assert_eq!(saved.name, "My Skin");
    assert_eq!(saved.variant, "slim");
    assert_eq!(saved.source, SAVED_SKIN_SOURCE);
    assert_eq!(saved.byte_size, normalized.png_bytes.len());
    assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
    assert_texture_key(&saved.texture_key);
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(
        cache_control.as_deref(),
        Some(SAVED_SKIN_FILE_CACHE_CONTROL)
    );
    assert_eq!(file_bytes, normalized.png_bytes);
}

#[tokio::test]
async fn skin_saved_save_uses_normalized_slim_suggestion_when_variant_is_omitted() {
    let fixture = TestFixture::new("saved-save-slim-suggestion", "ConfigUser");
    let png = test_slim_skin_png();

    let saved = fixture
        .save_skin("Detected Slim", None, png)
        .await
        .expect("save skin")
        .0;

    assert_eq!(saved.variant, "slim");
}

#[tokio::test]
async fn skin_saved_save_selects_available_cape() {
    let fixture = TestFixture::new("saved-save-cape", "ConfigUser");
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

    let saved = handle_save_skin(
        &fixture.state,
        SaveSkinQuery {
            name: Some("Cape Skin".to_string()),
            variant: None,
            cape_id: Some("cape-id".to_string()),
            source: None,
        },
        Body::from(test_skin_png(SKIN_WIDTH, SKIN_HEIGHT)),
    )
    .await
    .expect("save skin with cape")
    .0;
    let listed = fixture.saved_skins().await.expect("saved skins").0;

    assert_eq!(saved.cape_id.as_deref(), Some("cape-id"));
    assert_eq!(listed.skins, vec![saved]);
}

#[tokio::test]
async fn skin_saved_save_rejects_unavailable_cape() {
    let fixture = TestFixture::new("saved-save-unavailable-cape", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile_with_capes(
            "MinecraftName",
            Vec::new(),
            vec![minecraft_cape(
                "owned-cape",
                "ACTIVE",
                "https://textures.minecraft.net/texture/capeTexture",
            )],
        ))
        .await;

    let error = handle_save_skin(
        &fixture.state,
        SaveSkinQuery {
            name: Some("Cape Skin".to_string()),
            variant: None,
            cape_id: Some("missing-cape".to_string()),
            source: None,
        },
        Body::from(test_skin_png(SKIN_WIDTH, SKIN_HEIGHT)),
    )
    .await
    .expect_err("unavailable cape should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft cape is not available for this account",
            "status": "minecraft_cape_unavailable",
        }),
    );
}

#[tokio::test]
async fn skin_saved_duplicate_texture_key_updates_metadata() {
    let fixture = TestFixture::new("saved-duplicate", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);

    let first = fixture
        .save_skin("First", None, png.clone())
        .await
        .expect("first save")
        .0;
    let second = fixture
        .save_skin("Second", Some("slim".to_string()), png)
        .await
        .expect("second save")
        .0;
    let listed = fixture.saved_skins().await.expect("saved skins").0;

    assert_eq!(first.texture_key, second.texture_key);
    assert_eq!(first.created_at, second.created_at);
    assert!(second.updated_at >= first.updated_at);
    assert_eq!(second.name, "Second");
    assert_eq!(second.variant, "slim");
    assert_eq!(listed.skins, vec![second]);
}

#[tokio::test]
async fn skin_saved_update_metadata_changes_name_and_variant() {
    let fixture = TestFixture::new("saved-update-metadata", "ConfigUser");
    let saved = fixture
        .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let updated = fixture
        .update_saved_skin(
            &saved.texture_key,
            serde_json::json!({
                "name": " Renamed Skin ",
                "variant": "slim"
            }),
        )
        .await
        .expect("update skin")
        .0;
    let listed = fixture.saved_skins().await.expect("saved skins").0;

    assert_eq!(updated.texture_key, saved.texture_key);
    assert_eq!(updated.created_at, saved.created_at);
    assert!(updated.updated_at >= saved.updated_at);
    assert_eq!(updated.name, "Renamed Skin");
    assert_eq!(updated.variant, "slim");
    assert_eq!(updated.cape_id, saved.cape_id);
    assert_eq!(updated.applied_at, saved.applied_at);
    assert_eq!(updated.byte_size, saved.byte_size);
    assert_eq!(listed.skins, vec![updated]);
}

#[tokio::test]
async fn skin_saved_update_metadata_selects_and_clears_available_cape() {
    let fixture = TestFixture::new("saved-update-cape", "ConfigUser");
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
        .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let with_cape = fixture
        .update_saved_skin(
            &saved.texture_key,
            serde_json::json!({ "cape_id": "cape-id" }),
        )
        .await
        .expect("select cape")
        .0;
    let without_cape = fixture
        .update_saved_skin(&saved.texture_key, serde_json::json!({ "cape_id": null }))
        .await
        .expect("clear cape")
        .0;

    assert_eq!(with_cape.cape_id.as_deref(), Some("cape-id"));
    assert_eq!(without_cape.cape_id, None);
}

#[tokio::test]
async fn skin_saved_update_metadata_rejects_invalid_values() {
    let fixture = TestFixture::new("saved-update-invalid", "ConfigUser");
    let saved = fixture
        .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let invalid_name = fixture
        .update_saved_skin(
            &saved.texture_key,
            serde_json::json!({ "name": "bad/name" }),
        )
        .await
        .expect_err("invalid name should fail");
    let invalid_variant = fixture
        .update_saved_skin(&saved.texture_key, serde_json::json!({ "variant": "wide" }))
        .await
        .expect_err("invalid variant should fail");
    let invalid_key = fixture
        .update_saved_skin(
            "../not-a-texture-key",
            serde_json::json!({ "name": "Valid" }),
        )
        .await
        .expect_err("invalid key should fail");
    let missing = fixture
        .update_saved_skin(&"0".repeat(64), serde_json::json!({ "name": "Missing" }))
        .await
        .expect_err("missing skin should fail");

    assert_eq!(invalid_name.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_name.1.0,
        serde_json::json!({ "error": "skin name contains unsupported characters" })
    );
    assert_eq!(invalid_variant.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_variant.1.0,
        serde_json::json!({ "error": "skin variant must be classic or slim" })
    );
    assert_eq!(invalid_key.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_key.1.0,
        serde_json::json!({ "error": "invalid texture key" })
    );
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_eq!(
        missing.1.0,
        serde_json::json!({ "error": "saved skin not found" })
    );
}

#[tokio::test]
async fn skin_saved_replace_texture_changes_identity_and_file() {
    let fixture = TestFixture::new("saved-replace-texture", "ConfigUser");
    let saved = fixture
        .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    let replacement_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let replacement_normalized =
        normalize_skin_png(&replacement_png).expect("replacement normalized");

    let updated = fixture
        .replace_saved_skin_texture(
            &saved.texture_key,
            ReplaceSavedSkinTextureQuery {
                name: Some(" Replaced Skin ".to_string()),
                variant: Some("slim".to_string()),
                ..ReplaceSavedSkinTextureQuery::default()
            },
            replacement_png,
        )
        .await
        .expect("replace texture")
        .0;
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let replacement_file = fixture
        .saved_skin_file(&updated.texture_key)
        .await
        .expect("replacement file");
    let old_file = fixture
        .saved_skin_file(&saved.texture_key)
        .await
        .expect_err("old texture key should not be listed");

    assert_ne!(updated.texture_key, saved.texture_key);
    assert_eq!(
        updated.texture_key,
        texture_key(&replacement_normalized.png_bytes)
    );
    assert_eq!(updated.created_at, saved.created_at);
    assert!(updated.updated_at >= saved.updated_at);
    assert_eq!(updated.name, "Replaced Skin");
    assert_eq!(updated.variant, "slim");
    assert_eq!(updated.source, saved.source);
    assert_eq!(updated.cape_id, saved.cape_id);
    assert_eq!(updated.applied_at, None);
    assert_eq!(updated.byte_size, replacement_normalized.png_bytes.len());
    assert_eq!(listed.skins, vec![updated.clone()]);
    assert_eq!(
        response_bytes(replacement_file).await,
        replacement_normalized.png_bytes
    );
    assert_eq!(old_file.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn skin_saved_replace_texture_clears_stale_applied_state() {
    let fixture = TestFixture::new("saved-replace-clears-applied", "ConfigUser");
    let saved = fixture
        .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;
    fixture
        .state
        .skins()
        .mark_applied(&saved.texture_key)
        .expect("mark skin applied");

    let updated = fixture
        .replace_saved_skin_texture(
            &saved.texture_key,
            ReplaceSavedSkinTextureQuery {
                name: Some(saved.name.clone()),
                variant: Some(saved.variant.clone()),
                ..ReplaceSavedSkinTextureQuery::default()
            },
            test_slim_skin_png(),
        )
        .await
        .expect("replace texture")
        .0;

    assert_ne!(updated.texture_key, saved.texture_key);
    assert_eq!(updated.applied_at, None);
}

#[tokio::test]
async fn skin_saved_replace_texture_retargets_pending_apply() {
    let fixture = TestFixture::new("saved-replace-retargets-pending", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin(
            "Queued",
            None,
            test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 29),
        )
        .await
        .expect("save skin")
        .0;
    let _ = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue apply");

    let updated = fixture
        .replace_saved_skin_texture(
            &saved.texture_key,
            ReplaceSavedSkinTextureQuery {
                name: Some("Queued Replacement".to_string()),
                variant: Some("slim".to_string()),
                ..ReplaceSavedSkinTextureQuery::default()
            },
            test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 53),
        )
        .await
        .expect("replace texture")
        .0;
    let listed = fixture.saved_skins().await.expect("saved skins").0;

    assert_ne!(updated.texture_key, saved.texture_key);
    assert_eq!(
        listed.pending_apply_texture_key.as_deref(),
        Some(updated.texture_key.as_str())
    );
}

#[tokio::test]
async fn skin_saved_delete_removes_local_skin() {
    let fixture = TestFixture::new("saved-delete", "ConfigUser");
    let saved = fixture
        .save_skin("Delete Me", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect("save skin")
        .0;

    let deleted = fixture
        .delete_saved_skin(&saved.texture_key)
        .await
        .expect("delete skin")
        .0;
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let file_error = fixture
        .saved_skin_file(&saved.texture_key)
        .await
        .expect_err("file should be gone");

    assert_eq!(deleted, serde_json::json!({ "status": "deleted" }));
    assert!(listed.skins.is_empty());
    assert_eq!(file_error.0, StatusCode::NOT_FOUND);
    assert_eq!(
        file_error.1.0,
        serde_json::json!({ "error": "saved skin not found" })
    );
}

#[tokio::test]
async fn skin_saved_delete_clears_matching_pending_apply() {
    let fixture = TestFixture::new("saved-delete-clears-pending", "ConfigUser");
    fixture
        .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
        .await;
    let saved = fixture
        .save_skin(
            "Queued Delete",
            None,
            test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 31),
        )
        .await
        .expect("save skin")
        .0;
    let _ = fixture
        .queue_saved_skin_apply(&saved.texture_key)
        .await
        .expect("queue apply");
    let listed_before = fixture.saved_skins().await.expect("saved skins").0;

    let _ = fixture
        .delete_saved_skin(&saved.texture_key)
        .await
        .expect("delete queued skin");
    let listed_after = fixture.saved_skins().await.expect("saved skins").0;

    assert_eq!(
        listed_before.pending_apply_texture_key.as_deref(),
        Some(saved.texture_key.as_str())
    );
    assert_eq!(listed_after.pending_apply_texture_key, None);
    assert!(listed_after.skins.is_empty());
}

#[tokio::test]
async fn skin_saved_delete_rejects_applied_skin() {
    let fixture = TestFixture::new("saved-delete-rejects-applied", "ConfigUser");
    let saved = fixture
        .save_skin(
            "Applied Delete",
            None,
            test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 43),
        )
        .await
        .expect("save skin")
        .0;
    fixture
        .state
        .skins()
        .mark_applied(&saved.texture_key)
        .expect("mark applied")
        .expect("saved skin should exist");

    let error = fixture
        .delete_saved_skin(&saved.texture_key)
        .await
        .expect_err("applied skin delete should fail");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let file = fixture
        .saved_skin_file(&saved.texture_key)
        .await
        .expect("applied skin file should remain readable");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "applied saved skin cannot be deleted; reset or apply another skin first"
        })
    );
    assert_eq!(listed.skins.len(), 1);
    assert_eq!(listed.skins[0].texture_key, saved.texture_key);
    assert!(listed.skins[0].applied_at.is_some());
    assert_eq!(file.status(), StatusCode::OK);
}

#[tokio::test]
async fn skin_saved_delete_rejects_invalid_texture_key() {
    let fixture = TestFixture::new("saved-invalid-delete", "ConfigUser");

    let error = fixture
        .delete_saved_skin("../not-a-texture-key")
        .await
        .expect_err("invalid key should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "invalid texture key" })
    );
}

#[tokio::test]
async fn skin_saved_rejects_invalid_name() {
    let fixture = TestFixture::new("saved-invalid-name", "ConfigUser");

    let error = fixture
        .save_skin("bad/name", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect_err("invalid name should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "skin name contains unsupported characters" })
    );
}

#[tokio::test]
async fn skin_saved_read_error_is_bounded_json() {
    let fixture = TestFixture::new("saved-read-error", "ConfigUser");
    let skin_dir = fixture.root.join("skins");
    fs::create_dir_all(&skin_dir).expect("create skin dir");
    fs::write(skin_dir.join("index.json"), "{not-json").expect("write bad index");

    let error = fixture
        .saved_skins()
        .await
        .expect_err("bad index should fail");

    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Could not read saved skins. Check app data permissions and try again."
        })
    );
}

#[tokio::test]
async fn skin_saved_write_error_is_bounded_json() {
    let fixture = TestFixture::new("saved-write-error", "ConfigUser");
    let skin_dir = fixture.root.join("skins");
    fs::create_dir_all(&skin_dir).expect("create skin dir");
    fs::write(skin_dir.join("files"), "blocking file").expect("write blocking file");

    let error = fixture
        .save_skin("Blocked", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
        .await
        .expect_err("blocked file dir should fail");

    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Could not update saved skins. Check app data permissions and try again."
        })
    );
}

#[tokio::test]
async fn skin_profile_save_from_profile_downloads_normalizes_and_saves_active_skin() {
    let fixture = TestFixture::new("profile-save-active", "ConfigUser");
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
                    "SLIM",
                ),
            ],
        ))
        .await;

    let saved = fixture
        .save_skin_from_profile(
            SaveSkinFromProfileRequest::default(),
            texture_prefix.clone(),
        )
        .await
        .expect("save profile skin")
        .0;
    let request = requests.recv().await.expect("texture request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let file = fixture
        .saved_skin_file(&saved.texture_key)
        .await
        .expect("saved skin file");

    assert_eq!(request.path, "/texture/activeTexture123");
    assert_eq!(request.accept.as_deref(), Some("image/png"));
    assert_eq!(request.user_agent.as_deref(), Some(AXIAL_USER_AGENT));
    assert_eq!(saved.name, "MinecraftName profile skin");
    assert_eq!(normalized.variant_suggestion, "classic");
    assert_eq!(saved.variant, normalized.variant_suggestion);
    assert_eq!(saved.source, SAVED_SKIN_PROFILE_SOURCE);
    assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
    assert_eq!(saved.byte_size, normalized.png_bytes.len());
    assert_eq!(listed.skins, vec![saved.clone()]);
    assert_eq!(response_bytes(file).await, normalized.png_bytes);
}

#[tokio::test]
async fn skin_profile_save_from_profile_reuses_profile_file_cache() {
    let fixture = TestFixture::new("profile-save-reuses-profile-cache", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin(
                "active",
                "ACTIVE",
                &format!("{texture_prefix}activeTexture123"),
                "classic",
            )],
        ))
        .await;

    let preview = fixture
        .profile_file(texture_prefix.clone())
        .await
        .expect("profile skin preview");
    let request = requests.recv().await.expect("texture request");
    assert_eq!(response_bytes(preview).await, normalized.png_bytes);

    let saved = fixture
        .save_skin_from_profile(SaveSkinFromProfileRequest::default(), texture_prefix)
        .await
        .expect("save profile skin from cache")
        .0;

    assert_eq!(request.path, "/texture/activeTexture123");
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(saved.name, "MinecraftName profile skin");
    assert_eq!(saved.source, SAVED_SKIN_PROFILE_SOURCE);
    assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
    assert_eq!(saved.byte_size, normalized.png_bytes.len());
}

#[tokio::test]
async fn skin_profile_save_from_profile_accepts_name_and_variant_override() {
    let fixture = TestFixture::new("profile-save-overrides", "ConfigUser");
    let (texture_prefix, mut requests) = skin_profile_texture_test_server(
        SkinProfileTextureServerMode::Png(test_skin_png(SKIN_WIDTH, SKIN_HEIGHT)),
    )
    .await;
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin(
                "active",
                "ACTIVE",
                &format!("{texture_prefix}activeTexture123"),
                "classic",
            )],
        ))
        .await;

    let saved = fixture
        .save_skin_from_profile(
            SaveSkinFromProfileRequest {
                name: Some("  Profile Copy  ".to_string()),
                variant: Some("SLIM".to_string()),
                mark_current: None,
            },
            texture_prefix,
        )
        .await
        .expect("save profile skin")
        .0;
    let _ = requests.recv().await.expect("texture request");

    assert_eq!(saved.name, "Profile Copy");
    assert_eq!(saved.variant, "slim");
}

#[tokio::test]
async fn skin_profile_save_from_profile_missing_active_account_returns_bounded_error() {
    let fixture = TestFixture::new("profile-save-missing-active", "ConfigUser");

    let error = fixture
        .save_skin_from_profile(
            SaveSkinFromProfileRequest::default(),
            "http://127.0.0.1:9/texture/".to_string(),
        )
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
async fn skin_profile_save_from_profile_requires_sane_texture_url() {
    let fixture = TestFixture::new("profile-save-bad-texture", "ConfigUser");
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
        .save_skin_from_profile(
            SaveSkinFromProfileRequest::default(),
            "http://127.0.0.1:9/texture/".to_string(),
        )
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

#[tokio::test]
async fn skin_profile_save_from_profile_bounds_texture_download_size() {
    let fixture = TestFixture::new("profile-save-oversized-texture", "ConfigUser");
    let (texture_prefix, mut requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
    fixture
        .add_minecraft_account(test_profile(
            "MinecraftName",
            vec![minecraft_skin(
                "active",
                "ACTIVE",
                &format!("{texture_prefix}activeTexture123"),
                "slim",
            )],
        ))
        .await;

    let error = fixture
        .save_skin_from_profile(SaveSkinFromProfileRequest::default(), texture_prefix)
        .await
        .expect_err("oversized texture should fail");
    let _ = requests.recv().await.expect("texture request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;

    assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft profile skin is too large",
            "status": "minecraft_profile_skin_too_large",
        })
    );
    assert!(listed.skins.is_empty());
}

#[tokio::test]
async fn skin_username_save_downloads_normalizes_and_saves_session_skin() {
    let fixture = TestFixture::new("username-save-success", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("slim".to_string()),
            cape_url: None,
        })
        .await;

    let saved = fixture
        .save_skin_from_username(
            SaveSkinFromUsernameRequest {
                username: "  QueryUser  ".to_string(),
                name: None,
                variant: None,
            },
            profile_endpoint,
            session_endpoint,
            texture_prefix.clone(),
        )
        .await
        .expect("save username skin")
        .0;
    let profile_request = profile_requests.recv().await.expect("profile request");
    let session_request = profile_requests.recv().await.expect("session request");
    let texture_request = texture_requests.recv().await.expect("texture request");
    let listed = fixture.saved_skins().await.expect("saved skins").0;
    let file = fixture
        .saved_skin_file(&saved.texture_key)
        .await
        .expect("saved skin file");

    assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
    assert_eq!(
        session_request.path,
        "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
    );
    assert_eq!(profile_request.accept.as_deref(), Some("application/json"));
    assert_eq!(session_request.accept.as_deref(), Some("application/json"));
    assert_eq!(
        profile_request.user_agent.as_deref(),
        Some(AXIAL_USER_AGENT)
    );
    assert_eq!(
        session_request.user_agent.as_deref(),
        Some(AXIAL_USER_AGENT)
    );
    assert_eq!(texture_request.path, "/texture/usernameTexture123");
    assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
    assert_eq!(saved.name, "ResolvedName skin");
    assert_eq!(normalized.variant_suggestion, "classic");
    assert_eq!(saved.variant, normalized.variant_suggestion);
    assert_eq!(saved.source, SAVED_SKIN_USERNAME_SOURCE);
    assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
    assert_eq!(saved.byte_size, normalized.png_bytes.len());
    assert_eq!(listed.skins, vec![saved.clone()]);
    assert_eq!(response_bytes(file).await, normalized.png_bytes);
}

#[tokio::test]
async fn skin_username_save_reuses_lookup_skin_file_cache() {
    let fixture = TestFixture::new("username-save-reuses-lookup-cache", "ConfigUser");
    let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("classic".to_string()),
            cape_url: None,
        })
        .await;

    let preview = fixture
        .lookup_file(
            "QueryUser",
            None,
            profile_endpoint.clone(),
            session_endpoint.clone(),
            texture_prefix.clone(),
        )
        .await
        .expect("lookup username skin preview");
    let profile_request = profile_requests.recv().await.expect("profile request");
    let session_request = profile_requests.recv().await.expect("session request");
    let texture_request = texture_requests.recv().await.expect("texture request");
    assert_eq!(response_bytes(preview).await, normalized.png_bytes);

    let saved = fixture
        .save_skin_from_username(
            SaveSkinFromUsernameRequest {
                username: "QueryUser".to_string(),
                name: None,
                variant: None,
            },
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("save username skin from lookup cache")
        .0;

    assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
    assert_eq!(
        session_request.path,
        "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
    );
    assert_eq!(texture_request.path, "/texture/usernameTexture123");
    assert!(matches!(
        profile_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        texture_requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(saved.name, "ResolvedName skin");
    assert_eq!(saved.source, SAVED_SKIN_USERNAME_SOURCE);
    assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
    assert_eq!(saved.byte_size, normalized.png_bytes.len());
}

#[tokio::test]
async fn skin_username_save_accepts_name_and_variant_override() {
    let fixture = TestFixture::new("username-save-overrides", "ConfigUser");
    let png = test_slim_skin_png();
    let normalized = normalize_skin_png(&png).expect("normalized skin");
    let (texture_prefix, mut texture_requests) =
        skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
    let (profile_endpoint, session_endpoint, mut profile_requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::Success {
            texture_url: format!("{texture_prefix}usernameTexture123"),
            model: Some("slim".to_string()),
            cape_url: None,
        })
        .await;

    let saved = fixture
        .save_skin_from_username(
            SaveSkinFromUsernameRequest {
                username: "QueryUser".to_string(),
                name: Some("  Username Copy  ".to_string()),
                variant: Some("CLASSIC".to_string()),
            },
            profile_endpoint,
            session_endpoint,
            texture_prefix,
        )
        .await
        .expect("save username skin")
        .0;
    let _ = profile_requests.recv().await.expect("profile request");
    let _ = profile_requests.recv().await.expect("session request");
    let _ = texture_requests.recv().await.expect("texture request");

    assert_eq!(normalized.variant_suggestion, "slim");
    assert_eq!(saved.name, "Username Copy");
    assert_eq!(saved.variant, "classic");
}

#[tokio::test]
async fn skin_username_save_invalid_username_returns_bad_request() {
    let fixture = TestFixture::new("username-save-invalid", "ConfigUser");

    let error = fixture
        .save_skin_from_username(
            SaveSkinFromUsernameRequest {
                username: "bad name".to_string(),
                name: None,
                variant: None,
            },
            "http://127.0.0.1:9/users/profiles/minecraft".to_string(),
            "http://127.0.0.1:9/session/minecraft/profile".to_string(),
            "http://127.0.0.1:9/texture/".to_string(),
        )
        .await
        .expect_err("invalid username should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "Letters, numbers, and underscores only." })
    );
}

#[tokio::test]
async fn skin_username_save_missing_player_returns_bounded_404() {
    let fixture = TestFixture::new("username-save-not-found", "ConfigUser");
    let (profile_endpoint, session_endpoint, mut requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::NotFound).await;

    let error = fixture
        .save_skin_from_username(
            SaveSkinFromUsernameRequest {
                username: "MissingUser".to_string(),
                name: None,
                variant: None,
            },
            profile_endpoint,
            session_endpoint,
            "http://127.0.0.1:9/texture/".to_string(),
        )
        .await
        .expect_err("missing player should fail");
    let request = requests.recv().await.expect("profile request");

    assert_eq!(request.path, "/users/profiles/minecraft/MissingUser");
    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft player not found",
            "status": "minecraft_player_not_found",
        })
    );
}

#[tokio::test]
async fn skin_username_save_profile_without_skin_returns_bounded_conflict() {
    let fixture = TestFixture::new("username-save-missing-skin", "ConfigUser");
    let (profile_endpoint, session_endpoint, mut requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::MissingSkin).await;

    let error = fixture
        .save_skin_from_username(
            SaveSkinFromUsernameRequest {
                username: "NoSkinUser".to_string(),
                name: None,
                variant: None,
            },
            profile_endpoint,
            session_endpoint,
            "http://127.0.0.1:9/texture/".to_string(),
        )
        .await
        .expect_err("profile without skin should fail");
    let _ = requests.recv().await.expect("profile request");
    let _ = requests.recv().await.expect("session request");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft player profile does not have a usable skin texture",
            "status": "minecraft_username_skin_missing",
        })
    );
}

#[tokio::test]
async fn skin_username_save_malformed_textures_property_returns_bounded_conflict() {
    let fixture = TestFixture::new("username-save-malformed-textures", "ConfigUser");
    let (profile_endpoint, session_endpoint, mut requests) =
        minecraft_username_test_server(MinecraftUsernameServerMode::MalformedTextures).await;

    let error = fixture
        .save_skin_from_username(
            SaveSkinFromUsernameRequest {
                username: "BrokenUser".to_string(),
                name: None,
                variant: None,
            },
            profile_endpoint,
            session_endpoint,
            "http://127.0.0.1:9/texture/".to_string(),
        )
        .await
        .expect_err("malformed textures should fail");
    let _ = requests.recv().await.expect("profile request");
    let _ = requests.recv().await.expect("session request");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Minecraft player profile skin textures are malformed",
            "status": "minecraft_username_skin_malformed",
        })
    );
}
