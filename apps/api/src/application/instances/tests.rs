use super::*;
use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
use axum::http::{HeaderValue, header};
use croopor_config::{AppPaths, ConfigStore, InstanceStore};
use croopor_launcher::{LaunchSessionRecord, LaunchState, SessionId};
use croopor_performance::PerformanceManager;
use std::{collections::HashMap, fs, io, path::Path as FsPath, sync::Arc};

#[test]
fn instance_write_error_mapper_preserves_safe_status_messages() {
    let cases = [
        (
            io::ErrorKind::NotFound,
            "instance not found",
            StatusCode::NOT_FOUND,
            "instance not found",
        ),
        (
            io::ErrorKind::AlreadyExists,
            "an instance with this name already exists",
            StatusCode::CONFLICT,
            "an instance with this name already exists",
        ),
        (
            io::ErrorKind::InvalidInput,
            "version_id is required",
            StatusCode::BAD_REQUEST,
            "version_id is required",
        ),
    ];

    for (kind, store_message, expected_status, expected_message) in cases {
        let (status, Json(body)) = instance_write_error_response(
            InstanceWriteOperation::Create,
            InstanceStoreError::Read(io::Error::new(kind, store_message)),
        );

        assert_eq!(status, expected_status);
        assert_bounded_error_body(&body, expected_message);
    }
}

#[test]
fn instance_write_error_mapper_bounds_internal_operation_errors() {
    let cases = [
        (
            InstanceWriteOperation::Create,
            "failed to initialize instance files: /home/zero/.config/Croopor/instances/new/logs",
            "Could not create the instance. Check app data permissions and try again.",
        ),
        (
            InstanceWriteOperation::Update,
            "failed to persist /home/zero/.config/Croopor/instances.json",
            "Could not save the instance. Check app data permissions and try again.",
        ),
        (
            InstanceWriteOperation::Delete,
            "failed to delete C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\instances\\old",
            "Could not delete the instance. Check app data permissions and try again.",
        ),
    ];

    for (operation, store_message, expected_message) in cases {
        let (status, Json(body)) = instance_write_error_response(
            operation,
            InstanceStoreError::Read(io::Error::other(store_message)),
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_bounded_error_body(&body, expected_message);
        let public_message = error_body_text(&body);
        assert!(!public_message.contains("/home/zero"));
        assert!(!public_message.contains("C:\\Users\\Zero"));
        assert!(!public_message.contains("instances.json"));
    }
}

#[test]
fn duplicate_instance_write_error_hides_layout_and_persist_paths() {
    let store_message = concat!(
        "failed to duplicate instance files: ",
        "/home/zero/.config/Croopor/instances/source/mods/example.jar; ",
        "failed to roll back persisted instance: ",
        "C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\config\\instances.json"
    );

    let (status, Json(body)) = instance_write_error_response(
        InstanceWriteOperation::Duplicate,
        InstanceStoreError::Read(io::Error::other(store_message)),
    );

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_bounded_error_body(
        &body,
        "Could not duplicate the instance. Check app data permissions and try again.",
    );
    let public_message = error_body_text(&body);
    for hidden_fragment in [
        "/home/zero",
        ".config",
        "C:\\Users\\Zero",
        "AppData",
        "example.jar",
        "instances.json",
        "failed to duplicate instance files",
    ] {
        assert!(
            !public_message.contains(hidden_fragment),
            "{hidden_fragment:?} leaked in {public_message:?}"
        );
    }
}

#[test]
fn instance_folder_prepare_error_response_bounds_public_message() {
    assert_instance_folder_error_response_is_bounded(
        instance_folder_prepare_error_response,
        "Could not prepare the instance folder. Check app data permissions and try again.",
    );
}

#[test]
fn instance_folder_open_error_response_bounds_public_message() {
    assert_instance_folder_error_response_is_bounded(
        instance_folder_open_error_response,
        "Could not open the instance folder. Check desktop permissions and try again.",
    );
}

#[test]
fn instance_log_read_error_response_bounds_public_metadata_open_and_read_messages() {
    let cases = [
        "metadata failed for /home/zero/.config/Croopor/instances/test/logs/latest.log",
        "open failed for C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\instances\\test\\logs\\debug.log",
        "Permission denied (os error 13) while reading logs/latest.log",
    ];

    for internal_message in cases {
        let (status, Json(body)) =
            instance_log_read_error_response(io::Error::other(internal_message));

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_bounded_error_body(&body, INSTANCE_LOG_READ_ERROR_MESSAGE);
        let public_message = error_body_text(&body);
        for hidden_fragment in [
            "/home/zero",
            ".config",
            "C:\\Users\\Zero",
            "AppData",
            "Permission denied",
            "os error 13",
            "latest.log",
            "debug.log",
            "logs/",
            "\\logs\\",
        ] {
            assert!(
                !public_message.contains(hidden_fragment),
                "{hidden_fragment:?} leaked in {public_message:?}"
            );
        }
    }
}

#[test]
fn instance_folder_resolver_returns_root_when_subfolder_is_omitted() {
    let game_dir = FsPath::new("/tmp/croopor-instance");

    assert_eq!(
        resolve_instance_folder(game_dir, None).expect("resolve root"),
        game_dir
    );
}

#[test]
fn instance_folder_resolver_accepts_allowed_subfolder() {
    let game_dir = FsPath::new("/tmp/croopor-instance");

    assert_eq!(
        resolve_instance_folder(game_dir, Some("mods")).expect("resolve mods"),
        game_dir.join("mods")
    );
}

#[test]
fn instance_folder_resolver_rejects_unknown_subfolder() {
    let game_dir = FsPath::new("/tmp/croopor-instance");

    assert_eq!(
        resolve_instance_folder(game_dir, Some("versions")),
        Err("invalid instance folder")
    );
}

#[test]
fn instance_folder_resolver_rejects_traversal_like_subfolders() {
    let game_dir = FsPath::new("/tmp/croopor-instance");

    for subfolder in ["..", "../mods", "mods/..", "mods/../logs", "mods\\..\\logs"] {
        assert_eq!(
            resolve_instance_folder(game_dir, Some(subfolder)),
            Err("invalid instance folder"),
            "{subfolder:?} should be rejected"
        );
    }
}

#[test]
fn resource_names_reject_path_traversal_hidden_and_control_names() {
    for name in ["latest.log", "2026-05-30-1.log.gz", "debug.log"] {
        assert!(is_safe_resource_name(name), "{name} should be accepted");
    }

    for name in [
        "",
        "   ",
        " World",
        "World ",
        ".",
        "..",
        ".hidden.log",
        "../latest.log",
        "nested/latest.log",
        "nested\\latest.log",
        "bad\nname.log",
    ] {
        assert!(!is_safe_resource_name(name), "{name:?} should be rejected");
    }
}

#[test]
fn log_scanner_returns_only_safe_instance_local_file_names() {
    let root = std::env::temp_dir().join(format!(
        "croopor-api-instance-logs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    let logs_dir = root.join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    fs::write(logs_dir.join("latest.log"), "latest").expect("write latest");
    fs::write(logs_dir.join("debug.log"), "debug").expect("write debug");
    fs::write(logs_dir.join(".hidden.log"), "hidden").expect("write hidden");
    fs::create_dir_all(logs_dir.join("nested")).expect("create nested dir");
    fs::write(logs_dir.join("nested").join("nested.log"), "nested").expect("write nested");

    let names = scan_instance_logs(&logs_dir)
        .into_iter()
        .map(|log| log.name)
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec!["latest.log".to_string(), "debug.log".to_string()]
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn instance_log_tail_rejects_unsafe_log_name() {
    let fixture = TestFixture::new("log-tail-invalid-name");
    let instance = fixture
        .state
        .instances()
        .add(
            "Tail invalid log".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");

    let (status, Json(body)) =
        handle_instance_log_tail(&fixture.state, &instance.id, "../latest.log")
            .await
            .expect_err("unsafe log name should fail");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_bounded_error_body(&body, "invalid log filename");
}

#[tokio::test]
async fn instance_log_tail_returns_bounded_truncated_tail() {
    let fixture = TestFixture::new("log-tail-truncated");
    let instance = fixture
        .state
        .instances()
        .add(
            "Tail truncated log".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let logs_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    let discarded = b"discarded";
    let mut bytes = discarded.to_vec();
    bytes.resize(discarded.len() + LOG_TAIL_LIMIT as usize, b't');
    fs::write(logs_dir.join("latest.log"), &bytes).expect("write log");

    let response = handle_instance_log_tail(&fixture.state, &instance.id, "latest.log")
        .await
        .expect("tail log");

    assert_eq!(response.name, "latest.log");
    assert_eq!(response.size, LOG_TAIL_LIMIT + discarded.len() as u64);
    assert!(response.truncated);
    assert_eq!(response.text.len(), LOG_TAIL_LIMIT as usize);
    assert!(response.text.bytes().all(|byte| byte == b't'));
}

#[tokio::test]
async fn instance_log_tail_redacts_sensitive_public_lines() {
    let fixture = TestFixture::new("log-tail-redacts-sensitive-lines");
    let instance = fixture
        .state
        .instances()
        .add(
            "Redacted log tail".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let logs_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    fs::write(
        logs_dir.join("latest.log"),
        "[Render thread/INFO]: Reloading ResourceManager: vanilla\nfailed for /home/alice/.minecraft java.exe --accessToken raw-secret-token -Xmx8192M provider_payload=provider-secret account_id=account-secret username=SecretPlayer\n",
    )
    .expect("write log");

    let response = handle_instance_log_tail(&fixture.state, &instance.id, "latest.log")
        .await
        .expect("tail log");

    assert!(response.text.contains("[Render thread/INFO]"));
    assert!(
        response
            .text
            .contains(crate::observability::PUBLIC_LOG_LINE_REDACTED)
    );
    for fragment in [
        "/home/alice",
        ".minecraft",
        "java.exe",
        "--accessToken",
        "raw-secret-token",
        "-Xmx8192M",
        "provider_payload",
        "provider-secret",
        "account_id",
        "account-secret",
        "username",
        "SecretPlayer",
    ] {
        assert!(
            !response.text.contains(fragment),
            "instance log tail leaked fragment {fragment:?}: {}",
            response.text
        );
    }
}

#[test]
fn instance_screenshot_names_reject_path_traversal_hidden_and_control_names() {
    for name in [
        "2026-05-31_12.00.00.png",
        "castle build.jpg",
        "base.jpeg",
        "nether.webp",
    ] {
        assert!(
            validate_screenshot_name(name).is_ok(),
            "{name} should be accepted"
        );
    }

    for name in [
        "",
        "   ",
        ".",
        "..",
        ".hidden.png",
        "../shot.png",
        "nested/shot.png",
        "nested\\shot.png",
        "bad\nshot.png",
        " shot.png",
        "shot.png ",
        "notes.txt",
    ] {
        let (status, Json(body)) =
            validate_screenshot_name(name).expect_err("invalid screenshot name should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
        assert_bounded_error_body(&body, "invalid screenshot filename");
    }
}

#[test]
fn instance_screenshot_content_type_maps_supported_extensions() {
    assert_eq!(screenshot_content_type("shot.png"), Some("image/png"));
    assert_eq!(screenshot_content_type("shot.JPG"), Some("image/jpeg"));
    assert_eq!(screenshot_content_type("shot.jpeg"), Some("image/jpeg"));
    assert_eq!(screenshot_content_type("shot.webp"), Some("image/webp"));
    assert_eq!(screenshot_content_type("shot.gif"), None);
}

#[tokio::test]
async fn instance_screenshot_file_serves_valid_local_image() {
    let fixture = TestFixture::new("screenshot-file");
    let instance = fixture
        .state
        .instances()
        .add(
            "Serve screenshots".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let screenshots_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("screenshots");
    fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");
    fs::write(screenshots_dir.join("shot.PNG"), [137, 80, 78, 71]).expect("write screenshot");

    let response = handle_instance_screenshot_file(&fixture.state, &instance.id, "shot.PNG")
        .await
        .expect("serve screenshot");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE),
        Some(&HeaderValue::from_static("image/png"))
    );
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("read screenshot body");
    assert_eq!(&body[..], &[137, 80, 78, 71]);

    let (status, Json(body)) =
        handle_instance_screenshot_file(&fixture.state, &instance.id, "../shot.PNG")
            .await
            .expect_err("traversal should fail");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_bounded_error_body(&body, "invalid screenshot filename");
}

#[tokio::test]
async fn instance_screenshot_file_rejects_too_large_image() {
    let fixture = TestFixture::new("screenshot-file-too-large");
    let instance = fixture
        .state
        .instances()
        .add(
            "Large screenshot".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let screenshots_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("screenshots");
    fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");
    let file =
        fs::File::create(screenshots_dir.join("too-large.png")).expect("create large screenshot");
    file.set_len(SCREENSHOT_FILE_MAX_BYTES + 1)
        .expect("size large screenshot");

    let (status, Json(body)) =
        handle_instance_screenshot_file(&fixture.state, &instance.id, "too-large.png")
            .await
            .expect_err("too-large screenshot should fail");

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_bounded_error_body(&body, "screenshot file is too large");
}

#[tokio::test]
async fn instance_screenshot_rename_reports_not_found_conflict_and_success() {
    let fixture = TestFixture::new("screenshot-rename");
    let instance = fixture
        .state
        .instances()
        .add(
            "Rename screenshots".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let screenshots_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("screenshots");
    fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");

    let (status, Json(body)) = handle_rename_instance_screenshot(
        &fixture.state,
        &instance.id,
        "missing.png",
        RenameScreenshotRequest {
            name: "target.png".to_string(),
        },
    )
    .await
    .expect_err("missing source should fail");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "screenshot not found");

    fs::write(screenshots_dir.join("source.png"), "source").expect("write source");
    fs::write(screenshots_dir.join("target.png"), "target").expect("write target");
    let (status, Json(body)) = handle_rename_instance_screenshot(
        &fixture.state,
        &instance.id,
        "source.png",
        RenameScreenshotRequest {
            name: "target.png".to_string(),
        },
    )
    .await
    .expect_err("existing target should fail");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_bounded_error_body(&body, "screenshot already exists");
    assert_eq!(
        fs::read_to_string(screenshots_dir.join("source.png")).expect("read source"),
        "source"
    );

    let (status, Json(body)) = handle_rename_instance_screenshot(
        &fixture.state,
        &instance.id,
        "source.png",
        RenameScreenshotRequest {
            name: "renamed.webp".to_string(),
        },
    )
    .await
    .expect_err("changing screenshot type should fail");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_bounded_error_body(&body, "screenshot file type cannot change");
    assert_eq!(
        fs::read_to_string(screenshots_dir.join("source.png")).expect("read source"),
        "source"
    );

    let body = handle_rename_instance_screenshot(
        &fixture.state,
        &instance.id,
        "source.png",
        RenameScreenshotRequest {
            name: "renamed.png".to_string(),
        },
    )
    .await
    .expect("rename screenshot");
    assert_eq!(
        body,
        serde_json::json!({ "status": "ok", "name": "renamed.png" })
    );
    assert!(!screenshots_dir.join("source.png").exists());
    assert_eq!(
        fs::read_to_string(screenshots_dir.join("renamed.png")).expect("read renamed"),
        "source"
    );
}

#[tokio::test]
async fn instance_screenshot_delete_removes_only_named_file() {
    let fixture = TestFixture::new("screenshot-delete");
    let instance = fixture
        .state
        .instances()
        .add(
            "Delete screenshots".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let screenshots_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("screenshots");
    fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");
    fs::write(screenshots_dir.join("delete.png"), "deleted").expect("write deleted");
    fs::write(screenshots_dir.join("keep.png"), "kept").expect("write kept");

    let body = handle_delete_instance_screenshot(&fixture.state, &instance.id, "delete.png")
        .await
        .expect("delete screenshot");

    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(!screenshots_dir.join("delete.png").exists());
    assert_eq!(
        fs::read_to_string(screenshots_dir.join("keep.png")).expect("read kept"),
        "kept"
    );
}

#[test]
fn instance_screenshot_error_responses_do_not_leak_paths() {
    for mapper in [
        screenshot_file_read_error_response
            as fn(io::Error) -> (StatusCode, Json<serde_json::Value>),
        screenshot_file_write_error_response,
    ] {
        let (status, Json(body)) = mapper(io::Error::other(
            "failed for /home/zero/.config/Croopor/instances/test/screenshots/shot.png",
        ));

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let public_message = error_body_text(&body);
        assert!(!public_message.contains("/home/zero"));
        assert!(!public_message.contains("shot.png"));
        assert!(public_message.len() <= 180);
    }
}

#[test]
fn instance_mod_names_reject_path_traversal_hidden_and_non_mod_names() {
    for name in ["sodium.jar", "Sodium.JAR", "sodium.jar.disabled"] {
        assert!(validate_mod_name(name).is_ok(), "{name} should be accepted");
    }

    for name in [
        "",
        "   ",
        ".",
        "..",
        ".hidden.jar",
        "../mod.jar",
        "nested/mod.jar",
        "nested\\mod.jar",
        "bad\nmod.jar",
        "notes.txt",
        "mod.disabled",
    ] {
        let (status, Json(body)) =
            validate_mod_name(name).expect_err("invalid mod name should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
        assert_bounded_error_body(&body, "invalid mod filename");
    }
}

#[tokio::test]
async fn instance_mod_update_reports_not_found_conflict_and_success() {
    let fixture = TestFixture::new("mod-update");
    let instance = fixture
        .state
        .instances()
        .add(
            "Update mods".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");

    let (status, Json(body)) = handle_update_instance_mod(
        &fixture.state,
        &instance.id,
        "missing.jar",
        UpdateModRequest { enabled: false },
    )
    .await
    .expect_err("missing source should fail");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "mod not found");

    fs::write(mods_dir.join("source.jar.disabled"), "source").expect("write disabled mod");
    fs::write(mods_dir.join("source.jar"), "target").expect("write existing target");
    let (status, Json(body)) = handle_update_instance_mod(
        &fixture.state,
        &instance.id,
        "source.jar.disabled",
        UpdateModRequest { enabled: true },
    )
    .await
    .expect_err("existing target should fail");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_bounded_error_body(&body, "mod already exists");
    assert!(mods_dir.join("source.jar.disabled").is_file());

    fs::write(mods_dir.join("toggle.jar"), "toggle").expect("write enabled mod");
    let body = handle_update_instance_mod(
        &fixture.state,
        &instance.id,
        "toggle.jar",
        UpdateModRequest { enabled: false },
    )
    .await
    .expect("disable mod");
    assert_eq!(
        body,
        serde_json::json!({ "status": "ok", "name": "toggle.jar.disabled", "enabled": false })
    );
    assert!(!mods_dir.join("toggle.jar").exists());
    assert_eq!(
        fs::read_to_string(mods_dir.join("toggle.jar.disabled")).expect("read disabled mod"),
        "toggle"
    );

    let body = handle_update_instance_mod(
        &fixture.state,
        &instance.id,
        "toggle.jar.disabled",
        UpdateModRequest { enabled: true },
    )
    .await
    .expect("enable mod");
    assert_eq!(
        body,
        serde_json::json!({ "status": "ok", "name": "toggle.jar", "enabled": true })
    );
    assert!(mods_dir.join("toggle.jar").is_file());
    assert!(!mods_dir.join("toggle.jar.disabled").exists());
}

#[tokio::test]
async fn instance_mod_delete_removes_only_named_mod_file() {
    let fixture = TestFixture::new("mod-delete");
    let instance = fixture
        .state
        .instances()
        .add(
            "Delete mods".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("delete.jar"), "deleted").expect("write deleted mod");
    fs::write(mods_dir.join("keep.jar"), "kept").expect("write kept mod");

    let body = handle_delete_instance_mod(&fixture.state, &instance.id, "delete.jar")
        .await
        .expect("delete mod");

    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(!mods_dir.join("delete.jar").exists());
    assert_eq!(
        fs::read_to_string(mods_dir.join("keep.jar")).expect("read kept mod"),
        "kept"
    );
}

#[test]
fn instance_world_names_reject_path_traversal_hidden_and_control_names() {
    for name in ["World", "My World", "World-2026_05_31"] {
        assert!(
            validate_world_name(name).is_ok(),
            "{name} should be accepted"
        );
    }

    for name in [
        "",
        "   ",
        ".",
        "..",
        ".hidden",
        "../World",
        "nested/World",
        "nested\\World",
        "bad\nworld",
    ] {
        let (status, Json(body)) =
            validate_world_name(name).expect_err("invalid world name should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
        assert_bounded_error_body(&body, "invalid world name");
    }
}

#[tokio::test]
async fn instance_world_rename_reports_not_found_conflict_and_success() {
    let fixture = TestFixture::new("world-rename");
    let instance = fixture
        .state
        .instances()
        .add(
            "Rename worlds".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let saves_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("saves");

    let (status, Json(body)) = handle_rename_instance_world(
        &fixture.state,
        &instance.id,
        "Missing",
        RenameWorldRequest {
            name: "Target".to_string(),
        },
    )
    .await
    .expect_err("missing source should fail");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "world not found");

    fs::create_dir_all(saves_dir.join("World A")).expect("create source world");
    fs::create_dir_all(saves_dir.join("Existing")).expect("create existing world");
    let (status, Json(body)) = handle_rename_instance_world(
        &fixture.state,
        &instance.id,
        "World A",
        RenameWorldRequest {
            name: "Existing".to_string(),
        },
    )
    .await
    .expect_err("existing target should fail");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_bounded_error_body(&body, "world already exists");
    assert!(saves_dir.join("World A").is_dir());

    let body = handle_rename_instance_world(
        &fixture.state,
        &instance.id,
        "World A",
        RenameWorldRequest {
            name: "Renamed".to_string(),
        },
    )
    .await
    .expect("rename world");
    assert_eq!(
        body,
        serde_json::json!({ "status": "ok", "name": "Renamed" })
    );
    assert!(!saves_dir.join("World A").exists());
    assert!(saves_dir.join("Renamed").is_dir());
}

#[tokio::test]
async fn instance_world_delete_removes_only_named_world_directory() {
    let fixture = TestFixture::new("world-delete");
    let instance = fixture
        .state
        .instances()
        .add(
            "Delete worlds".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let saves_dir = fixture
        .state
        .instances()
        .game_dir(&instance.id)
        .join("saves");
    fs::create_dir_all(saves_dir.join("Delete Me")).expect("create deleted world");
    fs::write(saves_dir.join("Delete Me").join("level.dat"), "deleted").expect("write level");
    fs::create_dir_all(saves_dir.join("Keep Me")).expect("create kept world");
    fs::write(saves_dir.join("Keep Me").join("level.dat"), "kept").expect("write kept");

    let body = handle_delete_instance_world(&fixture.state, &instance.id, "Delete Me")
        .await
        .expect("delete world");

    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(!saves_dir.join("Delete Me").exists());
    assert_eq!(
        fs::read_to_string(saves_dir.join("Keep Me").join("level.dat")).expect("read kept"),
        "kept"
    );
}

#[tokio::test]
async fn instance_world_backup_copies_directory_to_instance_local_label() {
    let fixture = TestFixture::new("world-backup");
    let instance = fixture
        .state
        .instances()
        .add(
            "Backup worlds".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    let world_dir = game_dir.join("saves").join("Backup Me");
    fs::create_dir_all(world_dir.join("data")).expect("create world data");
    fs::write(world_dir.join("level.dat"), "level").expect("write level");
    fs::write(world_dir.join("data").join("map.dat"), "map").expect("write map");

    let body = handle_backup_instance_world(&fixture.state, &instance.id, "Backup Me")
        .await
        .expect("backup world");

    assert_eq!(body.status, "ok");
    assert!(body.backup.starts_with("Backup Me-"));
    assert_eq!(body.location, format!("backups/worlds/{}", body.backup));
    assert!(
        !body
            .location
            .contains(&game_dir.to_string_lossy().to_string())
    );

    let backup_dir = game_dir.join("backups").join("worlds").join(&body.backup);
    assert_eq!(
        fs::read_to_string(backup_dir.join("level.dat")).expect("read backup level"),
        "level"
    );
    assert_eq!(
        fs::read_to_string(backup_dir.join("data").join("map.dat")).expect("read backup map"),
        "map"
    );
    assert_eq!(
        fs::read_to_string(world_dir.join("level.dat")).expect("read original level"),
        "level"
    );
}

#[test]
fn instance_world_backup_cleans_temp_directory_after_copy_failure() {
    let root = test_root("world-backup-copy-failure");
    let source = root.join("source");
    let backup_root = root.join("backups").join("worlds");
    fs::create_dir_all(&source).expect("create source");
    fs::create_dir_all(&backup_root).expect("create backup root");

    let mut nested = source.clone();
    for index in 0..=WORLD_BACKUP_MAX_DEPTH + 1 {
        nested = nested.join(format!("d{index}"));
        fs::create_dir_all(&nested).expect("create nested source");
    }

    let error = copy_world_backup_staged(&source, &backup_root, "Failed Backup")
        .expect_err("deep source should fail bounded copy");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(!backup_root.join("Failed Backup").exists());
    let leftovers = fs::read_dir(&backup_root)
        .expect("read backup root")
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "backup temp entries should be removed after failure"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn instance_world_mutations_reject_active_instance() {
    let fixture = TestFixture::new("world-running-conflict");
    let instance = fixture
        .state
        .instances()
        .add(
            "Running worlds".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    fs::create_dir_all(game_dir.join("saves").join("World")).expect("create world");
    fixture
        .state
        .sessions()
        .insert(test_launch_record("active-world-session", &instance.id))
        .await;

    let (status, Json(body)) = handle_delete_instance_world(&fixture.state, &instance.id, "World")
        .await
        .expect_err("running instance should reject world mutation");

    assert_eq!(status, StatusCode::CONFLICT);
    assert_bounded_error_body(
        &body,
        "cannot change worlds while the instance is running; stop the game first",
    );
    assert!(game_dir.join("saves").join("World").is_dir());
}

#[tokio::test]
async fn update_instance_allows_unchanged_name_and_maps_name_collision_to_conflict() {
    let fixture = TestFixture::new("update-name-collision");
    let alpha = fixture
        .state
        .instances()
        .add(
            "Alpha".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add alpha");
    let beta = fixture
        .state
        .instances()
        .add(
            "Beta".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add beta");

    let updated = handle_update_instance(
        &fixture.state,
        &alpha.id,
        InstancePatch {
            name: Some(alpha.name.clone()),
            version_id: Some("1.21.2".to_string()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect("unchanged name update should succeed");
    assert_eq!(updated.name, "Alpha");
    assert_eq!(updated.version_id, "1.21.2");

    let (status, Json(body)) = handle_update_instance(
        &fixture.state,
        &alpha.id,
        InstancePatch {
            name: Some(beta.name.clone()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect_err("duplicate name update should fail");

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        serde_json::json!({ "error": "an instance with this name already exists" })
    );
    assert_eq!(
        fixture
            .state
            .instances()
            .get(&alpha.id)
            .expect("alpha remains")
            .name,
        "Alpha"
    );
}

#[tokio::test]
async fn update_instance_unknown_jvm_preset_resets_to_auto_without_echoing_raw_value() {
    let fixture = TestFixture::new("update-unknown-jvm-preset");
    let instance = fixture
        .state
        .instances()
        .add(
            "Preset tamper".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");

    let updated = handle_update_instance(
        &fixture.state,
        &instance.id,
        InstancePatch {
            jvm_preset: Some(r"C:\Users\Alice\java.exe --accessToken raw-secret-token".to_string()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect("unknown preset update should normalize");

    assert_eq!(updated.jvm_preset, "");
    assert_eq!(
        fixture
            .state
            .instances()
            .get(&instance.id)
            .expect("stored instance")
            .jvm_preset,
        ""
    );
    let public = serde_json::to_string(&updated).expect("serialize updated instance");
    for leaked in ["Alice", "java.exe", "accessToken", "raw-secret-token"] {
        assert!(
            !public.contains(leaked),
            "{leaked:?} leaked in update response: {public}"
        );
    }
}

#[tokio::test]
async fn update_instance_response_redacts_java_path_and_jvm_args() {
    let fixture = TestFixture::new("update-runtime-overrides-redacted");
    let instance = fixture
        .state
        .instances()
        .add(
            "Runtime override".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");

    let raw_java = r"C:\Users\Alice\.jdks\bad\bin\java.exe";
    let raw_args = "-Dtoken=raw-secret-token -javaagent:C:\\Users\\Alice\\agent.jar";
    let updated = handle_update_instance(
        &fixture.state,
        &instance.id,
        InstancePatch {
            java_path: Some(raw_java.to_string()),
            extra_jvm_args: Some(raw_args.to_string()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect("update runtime overrides");

    assert_eq!(updated.java_path, "");
    assert_eq!(updated.extra_jvm_args, "");
    let stored = fixture
        .state
        .instances()
        .get(&instance.id)
        .expect("stored instance");
    assert_eq!(stored.java_path, raw_java);
    assert_eq!(stored.extra_jvm_args, raw_args);

    let public = serde_json::to_string(&updated).expect("serialize updated instance");
    for leaked in [
        "Alice",
        "java.exe",
        "raw-secret-token",
        "javaagent",
        "agent.jar",
        "C:\\Users",
    ] {
        assert!(
            !public.contains(leaked),
            "{leaked:?} leaked in update response: {public}"
        );
    }
}

#[tokio::test]
async fn instance_crud_handlers_create_list_get_update_and_delete() {
    let fixture = TestFixture::new("crud-happy-path");
    fixture.configure_create_manifest(&["1.21.1", "1.21.2"]);

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Survival".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            icon: "grass".to_string(),
            accent: "#5aa469".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create instance");
    assert_eq!(created.name, "Survival");
    assert_eq!(created.version_id, "1.21.1");
    assert_eq!(created.icon, "grass");
    assert_eq!(created.accent, "#5aa469");

    let listed = handle_list_instances(&fixture.state).await;
    assert_eq!(listed.last_instance_id, None);
    assert_eq!(listed.instances.len(), 1);
    assert_eq!(listed.instances[0].instance.id, created.id);
    assert_eq!(listed.instances[0].instance.name, "Survival");
    assert!(!listed.instances[0].launchable);
    assert_eq!(listed.instances[0].status_detail, "version not installed");
    assert_eq!(listed.instances[0].launch_action.label, "Install");
    assert_eq!(
        listed.instances[0].launch_action.primary_action,
        croopor_config::LaunchPrimaryAction::Install
    );

    let fetched = handle_get_instance(&fixture.state, &created.id)
        .await
        .expect("get instance");
    assert_eq!(fetched, created.instance.clone());

    let updated = handle_update_instance(
        &fixture.state,
        &created.id,
        InstancePatch {
            name: Some("Skyblock".to_string()),
            version_id: Some("1.21.2".to_string()),
            max_memory_mb: Some(4096),
            icon: Some("cloud".to_string()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect("update instance");
    assert_eq!(updated.id, created.id);
    assert_eq!(updated.name, "Skyblock");
    assert_eq!(updated.version_id, "1.21.2");
    assert_eq!(updated.max_memory_mb, 4096);
    assert_eq!(updated.icon, "cloud");

    let game_dir = fixture.state.instances().game_dir(&created.id);
    fs::write(game_dir.join("logs").join("latest.log"), "started").expect("write log");

    let body = handle_delete_instance(&fixture.state, &created.id, HashMap::new())
        .await
        .expect("delete instance");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(fixture.state.instances().get(&created.id).is_none());
    assert!(!game_dir.exists());
}

#[tokio::test]
async fn create_instance_duplicate_name_gets_backend_owned_suffix() {
    let fixture = TestFixture::new("create-name-conflict");
    fixture.configure_create_manifest(&["1.21.1", "1.21.2"]);
    let original = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Survival".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            icon: String::new(),
            accent: String::new(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create original instance");
    assert_eq!(original.name, "Survival");

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Survival".to_string(),
            selection_id: "vanilla|1.21.2".to_string(),
            icon: String::new(),
            accent: String::new(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("duplicate name should be resolved by Application");

    assert_eq!(created.name, "Survival (1)");
    assert_eq!(created.version_id, "1.21.2");
    assert_eq!(fixture.state.instances().list().len(), 2);
}

#[tokio::test]
async fn create_instance_applies_initial_settings_and_supported_preset_in_backend() {
    let fixture = TestFixture::new("create-initial-settings");
    fixture.configure_create_manifest(&["1.21.1"]);

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Tuned".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            max_memory_mb: Some(6144),
            min_memory_mb: Some(1024),
            window_width: Some(1280),
            window_height: Some(720),
            art_seed: Some(42),
            jvm_preset_id: Some("performance".to_string()),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create tuned instance");

    assert_eq!(
        created.result.command,
        crate::state::contracts::CommandKind::CreateInstance
    );
    assert_eq!(created.max_memory_mb, 6144);
    assert_eq!(created.min_memory_mb, 1024);
    assert_eq!(created.window_width, 1280);
    assert_eq!(created.window_height, 720);
    assert_eq!(created.art_seed, 42);
    assert_eq!(created.jvm_preset, "performance");
    assert!(created.guardian_notice.is_none());
    assert_eq!(
        fixture
            .state
            .instances()
            .get(&created.id)
            .expect("stored instance")
            .jvm_preset,
        "performance"
    );
}

#[tokio::test]
async fn create_instance_unknown_preset_resets_to_auto_without_echoing_raw_value() {
    let fixture = TestFixture::new("create-unknown-preset");
    fixture.configure_create_manifest(&["1.21.1"]);

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Unknown preset".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            jvm_preset_id: Some(
                r"C:\Users\Alice\java.exe --accessToken raw-secret-token".to_string(),
            ),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create instance with unknown preset");

    assert_eq!(created.jvm_preset, "");
    assert_eq!(
        created
            .guardian_notice
            .as_ref()
            .expect("guardian notice")
            .state_id,
        "unknown_reset_to_auto"
    );
    let public = serde_json::to_string(&created).expect("serialize create response");
    for leaked in ["Alice", "java.exe", "accessToken", "raw-secret-token"] {
        assert!(
            !public.contains(leaked),
            "{leaked:?} leaked in create response: {public}"
        );
    }
}

#[tokio::test]
async fn create_instance_blank_preset_remains_auto_without_warning() {
    let fixture = TestFixture::new("create-blank-preset");
    fixture.configure_create_manifest(&["1.21.1"]);

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Blank preset".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            jvm_preset_id: Some("   ".to_string()),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create instance with blank preset");

    assert_eq!(created.jvm_preset, "");
    assert!(created.guardian_notice.is_none());
    assert_eq!(created.view_model.tone, "success");
}

#[tokio::test]
async fn create_instance_requires_backend_selection_id() {
    let fixture = TestFixture::new("create-selection-required");
    fixture.configure_create_manifest(&["1.21.1"]);

    let (status, Json(body)) = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "No selection".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect_err("missing selection should fail");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        serde_json::json!({ "error": "selection_id is required" })
    );
    assert!(fixture.state.instances().list().is_empty());
}

#[tokio::test]
async fn create_instance_rejects_unknown_vanilla_selection_without_echoing_raw_value() {
    let fixture = TestFixture::new("create-unknown-vanilla-selection");
    fixture.configure_create_manifest(&["1.21.1"]);

    let (status, Json(body)) = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Bad selector".to_string(),
            selection_id: r"vanilla|C:\Users\Alice\java.exe --accessToken raw-secret-token"
                .to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect_err("unknown vanilla selection should fail");

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        serde_json::json!({ "error": "Minecraft version is unavailable" })
    );
    let public = body.to_string();
    for leaked in [
        "Alice",
        "java.exe",
        "accessToken",
        "raw-secret-token",
        "C:\\Users",
    ] {
        assert!(
            !public.contains(leaked),
            "{leaked:?} leaked in create selection response: {public}"
        );
    }
    assert!(fixture.state.instances().list().is_empty());
}

#[tokio::test]
async fn create_instance_rejects_direct_loader_build_selection_without_echoing_raw_value() {
    let fixture = TestFixture::new("create-direct-loader-selection");
    fixture.configure_create_manifest(&["1.21.1"]);

    let (status, Json(body)) = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Direct loader".to_string(),
            selection_id:
                r"loader|net.fabricmc.fabric-loader|C:\Users\Alice\loader.jar --accessToken raw-secret-token"
                    .to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect_err("direct loader build selection should fail");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        serde_json::json!({ "error": "invalid create selection" })
    );
    let public = body.to_string();
    for leaked in [
        "Alice",
        "loader.jar",
        "accessToken",
        "raw-secret-token",
        "C:\\Users",
    ] {
        assert!(
            !public.contains(leaked),
            "{leaked:?} leaked in create loader selection response: {public}"
        );
    }
    assert!(fixture.state.instances().list().is_empty());
}

#[tokio::test]
async fn create_instance_view_returns_backend_authored_version_rows() {
    let fixture = TestFixture::new("create-view-version-rows");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["1.21.1", "1.21.2"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    for component in croopor_minecraft::fetch_components() {
        let versions = if component.id == croopor_minecraft::LoaderComponentId::Fabric {
            vec!["1.21.1"]
        } else {
            Vec::new()
        };
        write_supported_versions_cache(&library_dir, component.id, &versions);
    }

    let view = handle_create_instance_view(&fixture.state).await;

    let vanilla = view
        .versions
        .iter()
        .find(|row| row.source_id == "vanilla" && row.minecraft_version_id == "1.21.1")
        .expect("vanilla row");
    assert_eq!(vanilla.selection_id, "vanilla|1.21.1");
    assert_eq!(vanilla.download_state, "full");
    assert_eq!(vanilla.channel, "release");

    let fabric = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == croopor_minecraft::LoaderComponentId::Fabric.as_str()
                && row.minecraft_version_id == "1.21.1"
        })
        .expect("fabric row");
    assert_eq!(
        fabric.selection_id,
        "loader_minecraft|net.fabricmc.fabric-loader|1.21.1"
    );
    assert_eq!(fabric.download_state, "base");
    assert!(
        view.notices.is_empty(),
        "unexpected notices: {:?}",
        view.notices
    );
}

#[tokio::test]
async fn create_instance_vanilla_selection_returns_backend_queue_state() {
    let fixture = TestFixture::new("create-vanilla-queue");
    fixture
        .state
        .set_library_dir(fixture.root.join("library").to_string_lossy().to_string());
    write_version_manifest_cache(&fixture.root.join("library"), &["1.21.2"]);
    fixture
        .state
        .installs()
        .enqueue_queued_install(
            "busy-queue".to_string(),
            crate::state::InstallQueueSpec::vanilla("busy".to_string(), String::new()),
            crate::state::InstallQueuePlacement::Back,
        )
        .await;
    fixture
        .state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("reserve active queue slot");

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Queued".to_string(),
            selection_id: "vanilla|1.21.2".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create and queue install");

    assert_eq!(created.version_id, "1.21.2");
    let queued = created.queued_install.expect("queued install summary");
    assert_eq!(queued.state_id, "install_queued");
    assert_eq!(queued.kind, "vanilla");
    assert_eq!(queued.label, "Minecraft 1.21.2");
    assert!(queued.queue_id.is_some());
    assert!(created.install_queue.expect("install queue").items.len() >= 1);
}

#[tokio::test]
async fn create_instance_installed_vanilla_selection_does_not_queue_install() {
    let fixture = TestFixture::new("create-installed-vanilla");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Installed".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create installed vanilla instance");

    assert_eq!(created.version_id, "1.21.1");
    assert!(created.launchable);
    assert!(created.install_queue.is_none());
    assert!(created.queued_install.is_none());
    assert_eq!(created.view_model.state_id, "created");
}

#[tokio::test]
async fn create_instance_loader_selection_resolves_cached_build_and_queues_backend_install() {
    let fixture = TestFixture::new("create-loader-queue");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir(library_dir.to_string_lossy().to_string());
    fixture
        .state
        .installs()
        .enqueue_queued_install(
            "busy-loader-queue".to_string(),
            crate::state::InstallQueueSpec::vanilla("busy".to_string(), String::new()),
            crate::state::InstallQueuePlacement::Back,
        )
        .await;
    fixture
        .state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("reserve active queue slot");
    let build_id = write_fabric_loader_build_cache(&library_dir, "1.21.99", "0.16.14");

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Fabric queued".to_string(),
            selection_id: "loader_minecraft|net.fabricmc.fabric-loader|1.21.99".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create and queue loader install");

    assert_eq!(created.version_id, "fabric-loader-0.16.14-1.21.99");
    let queued = created.queued_install.expect("queued install summary");
    assert_eq!(queued.state_id, "install_queued");
    assert_eq!(queued.kind, "loader");
    assert_eq!(queued.label, "Fabric 0.16.14 for Minecraft 1.21.99");
    assert!(queued.queue_id.is_some());
    assert_eq!(build_id, "fabric:1.21.99:0.16.14");
}

#[tokio::test]
async fn duplicate_instance_existing_name_maps_to_conflict_json_error() {
    let fixture = TestFixture::new("duplicate-name-conflict");
    let source = fixture
        .state
        .instances()
        .add(
            "Source".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add source instance");
    fixture
        .state
        .instances()
        .add(
            "Existing".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add existing instance");

    let (status, Json(body)) = handle_duplicate_instance(
        &fixture.state,
        &source.id,
        Some(DuplicateInstanceRequest {
            name: Some("Existing".to_string()),
        }),
    )
    .await
    .expect_err("duplicate name should fail");

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        serde_json::json!({ "error": "an instance with this name already exists" })
    );
    assert_eq!(fixture.state.instances().list().len(), 2);
}

#[tokio::test]
async fn open_instance_folder_missing_instance_returns_not_found_json_error() {
    let fixture = TestFixture::new("open-folder-missing");

    let (status, Json(body)) =
        handle_open_instance_folder(&fixture.state, "missing", OpenFolderQuery { sub: None })
            .await
            .expect_err("missing open-folder should fail");

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "instance not found");
}

#[tokio::test]
async fn open_instance_folder_rejects_traversal_subfolder_without_creating_escape_path() {
    let fixture = TestFixture::new("open-folder-traversal");
    let instance = fixture
        .state
        .instances()
        .add(
            "Traversal".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance");
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    let escaped_dir = game_dir
        .parent()
        .expect("game dir parent")
        .join("escaped-open-folder");
    assert!(!escaped_dir.exists());

    let (status, Json(body)) = handle_open_instance_folder(
        &fixture.state,
        &instance.id,
        OpenFolderQuery {
            sub: Some("../escaped-open-folder".to_string()),
        },
    )
    .await
    .expect_err("traversal open-folder should fail");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_bounded_error_body(&body, "invalid instance folder");
    assert!(!escaped_dir.exists());
}

#[tokio::test]
async fn missing_instance_crud_handlers_return_not_found_json_error() {
    let fixture = TestFixture::new("missing-crud");

    let (status, Json(body)) = handle_get_instance(&fixture.state, "missing")
        .await
        .expect_err("missing get should fail");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "instance not found");

    let (status, Json(body)) = handle_update_instance(
        &fixture.state,
        "missing",
        InstancePatch {
            name: Some("Nope".to_string()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect_err("missing update should fail");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "instance not found");

    let (status, Json(body)) = handle_delete_instance(&fixture.state, "missing", HashMap::new())
        .await
        .expect_err("missing delete should fail");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "instance not found");
}

#[tokio::test]
async fn delete_instance_default_removes_files_and_keep_files_preserves_them() {
    let fixture = TestFixture::new("delete-files");
    let remove_files = fixture
        .state
        .instances()
        .add(
            "Remove files".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add remove-files instance");
    let remove_game_dir = fixture.state.instances().game_dir(&remove_files.id);
    fs::write(remove_game_dir.join("mods").join("example.jar"), "mod").expect("write mod");

    let body = handle_delete_instance(&fixture.state, &remove_files.id, HashMap::new())
        .await
        .expect("delete with default file removal");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(!remove_game_dir.exists());

    let keep_files = fixture
        .state
        .instances()
        .add(
            "Keep files".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add keep-files instance");
    let keep_game_dir = fixture.state.instances().game_dir(&keep_files.id);
    let keep_marker = keep_game_dir.join("saves").join("world").join("level.dat");
    fs::create_dir_all(keep_marker.parent().expect("marker parent")).expect("create world");
    fs::write(&keep_marker, "level").expect("write level");

    let body = handle_delete_instance(
        &fixture.state,
        &keep_files.id,
        HashMap::from([("keep_files".to_string(), "true".to_string())]),
    )
    .await
    .expect("delete while keeping files");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));

    assert!(fixture.state.instances().get(&keep_files.id).is_none());
    assert!(keep_marker.exists());
}

fn assert_bounded_error_body(body: &serde_json::Value, expected: &str) {
    let object = body.as_object().expect("error body should be an object");
    assert_eq!(object.len(), 1);
    assert_eq!(
        body.get("error").and_then(serde_json::Value::as_str),
        Some(expected)
    );
}

fn error_body_text(body: &serde_json::Value) -> &str {
    body.get("error")
        .and_then(serde_json::Value::as_str)
        .expect("error message should be a string")
}

fn assert_instance_folder_error_response_is_bounded(
    mapper: fn(io::Error) -> (StatusCode, Json<serde_json::Value>),
    expected_message: &str,
) {
    for internal_message in [
        "failed for /home/zero/.config/Croopor/instances/test/mods",
        "failed for C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\instances\\test\\logs",
        "Permission denied (os error 13)",
    ] {
        let (status, Json(body)) = mapper(io::Error::other(internal_message));

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_bounded_error_body(&body, expected_message);
        let public_message = error_body_text(&body);
        for hidden_fragment in [
            "/home/zero",
            ".config",
            "C:\\Users\\Zero",
            "AppData",
            "Permission denied",
            "os error 13",
        ] {
            assert!(
                !public_message.contains(hidden_fragment),
                "{hidden_fragment:?} leaked in {public_message:?}"
            );
        }
    }
}

struct TestFixture {
    state: AppState,
    root: PathBuf,
}

#[derive(serde::Serialize)]
struct TestCachedCatalog<T> {
    schema_version: u32,
    fetched_at_ms: i64,
    value: T,
}

fn write_fabric_loader_build_cache(
    library_dir: &FsPath,
    minecraft_version: &str,
    loader_version: &str,
) -> String {
    let component_id = croopor_minecraft::LoaderComponentId::Fabric;
    let build_id = croopor_minecraft::build_id_for(component_id, minecraft_version, loader_version);
    let version_id = croopor_minecraft::installed_version_id_for(
        component_id,
        minecraft_version,
        loader_version,
    );
    let index = croopor_minecraft::LoaderVersionIndex {
        component_id,
        builds: vec![croopor_minecraft::LoaderBuildRecord {
            subject_kind: croopor_minecraft::loaders::types::LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: "Fabric".to_string(),
            build_id: build_id.clone(),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
            version_id,
            build_meta: croopor_minecraft::LoaderBuildMetadata {
                selection: croopor_minecraft::LoaderSelectionMeta {
                    default_rank: 100,
                    reason: croopor_minecraft::LoaderSelectionReason::Recommended,
                    source: croopor_minecraft::LoaderSelectionSource::ExplicitApiFlag,
                },
                ..croopor_minecraft::LoaderBuildMetadata::default()
            },
            strategy: croopor_minecraft::LoaderInstallStrategy::FabricProfile,
            artifact_kind: croopor_minecraft::LoaderArtifactKind::ProfileJson,
            installability: croopor_minecraft::LoaderInstallability::Installable,
            install_source: croopor_minecraft::loaders::LoaderInstallSource::ProfileJson {
                url: "https://example.invalid/fabric-profile.json".to_string(),
            },
        }],
    };
    let cache = TestCachedCatalog {
        schema_version: 6,
        fetched_at_ms: chrono::Utc::now().timestamp_millis(),
        value: index,
    };
    let cache_dir = croopor_minecraft::loader_catalog_dir(library_dir);
    fs::create_dir_all(&cache_dir).expect("create loader cache dir");
    fs::write(
        cache_dir.join(format!(
            "component-{}-builds-{}.json",
            component_id.short_key(),
            minecraft_version
        )),
        serde_json::to_vec_pretty(&cache).expect("serialize cached loader catalog"),
    )
    .expect("write loader build cache");
    build_id
}

fn write_version_manifest_cache(library_dir: &FsPath, version_ids: &[&str]) {
    let cache_path = croopor_minecraft::version_manifest_cache_path(library_dir);
    fs::create_dir_all(cache_path.parent().expect("version manifest cache parent"))
        .expect("create version manifest cache dir");
    let versions = version_ids
        .iter()
        .enumerate()
        .map(|(index, version_id)| {
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "url": format!("https://example.invalid/{version_id}.json"),
                "time": format!("2026-01-{:02}T00:00:00+00:00", index + 1),
                "releaseTime": format!("2026-01-{:02}T00:00:00+00:00", index + 1),
                "sha1": "",
                "complianceLevel": 1
            })
        })
        .collect::<Vec<_>>();
    fs::write(
        cache_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "latest": {
                "release": version_ids.first().copied().unwrap_or("1.21.99"),
                "snapshot": version_ids.last().copied().unwrap_or("1.21.99")
            },
            "versions": versions
        }))
        .expect("serialize version manifest cache"),
    )
    .expect("write version manifest cache");
}

fn write_supported_versions_cache(
    library_dir: &FsPath,
    component_id: croopor_minecraft::LoaderComponentId,
    version_ids: &[&str],
) {
    let cache = TestCachedCatalog {
        schema_version: 6,
        fetched_at_ms: chrono::Utc::now().timestamp_millis(),
        value: version_ids
            .iter()
            .map(|version_id| croopor_minecraft::LoaderGameVersion {
                subject_kind: croopor_minecraft::VersionSubjectKind::MinecraftVersion,
                id: (*version_id).to_string(),
                release_time: String::new(),
                minecraft_meta: croopor_minecraft::MinecraftVersionMeta::default(),
                lifecycle: croopor_minecraft::LifecycleMeta::default(),
                stable_hint: Some(true),
            })
            .collect::<Vec<_>>(),
    };
    let cache_dir = croopor_minecraft::loader_catalog_dir(library_dir);
    fs::create_dir_all(&cache_dir).expect("create loader cache dir");
    fs::write(
        cache_dir.join(format!(
            "component-{}-supported-versions.json",
            component_id.short_key()
        )),
        serde_json::to_vec_pretty(&cache).expect("serialize cached supported versions"),
    )
    .expect("write supported versions cache");
}

fn write_installed_vanilla_version(library_dir: &FsPath, version_id: &str) {
    let version_dir = library_dir.join("versions").join(version_id);
    fs::create_dir_all(&version_dir).expect("create version dir");
    fs::write(
        version_dir.join(format!("{version_id}.json")),
        format!(
            r#"{{
                "id": "{version_id}",
                "type": "release",
                "releaseTime": "2026-01-01T00:00:00+00:00",
                "javaVersion": {{"component": "java-runtime-gamma", "majorVersion": 17}}
            }}"#
        ),
    )
    .expect("write version json");
    fs::write(version_dir.join(format!("{version_id}.jar")), "client jar").expect("write jar");
}

impl TestFixture {
    fn new(name: &str) -> Self {
        let root = test_root(name);
        let paths = test_paths(&root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
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

    fn configure_create_manifest(&self, version_ids: &[&str]) -> PathBuf {
        let library_dir = self.root.join("library");
        self.state
            .set_library_dir(library_dir.to_string_lossy().to_string());
        write_version_manifest_cache(&library_dir, version_ids);
        library_dir
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn test_root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "croopor-api-instances-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create test root");
    path
}

fn test_paths(root: &FsPath) -> AppPaths {
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

fn test_launch_record(session_id: &str, instance_id: &str) -> LaunchSessionRecord {
    LaunchSessionRecord {
        session_id: SessionId(session_id.to_string()),
        instance_id: instance_id.to_string(),
        version_id: "1.21.1".to_string(),
        launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
        benchmark: None,
        state: LaunchState::Queued,
        pid: None,
        process_started_at_ms: None,
        boot_completed_at_ms: None,
        boot_duration_ms: None,
        priority: None,
        exit_code: None,
        command: Vec::new(),
        java_path: None,
        natives_dir: None,
        failure: None,
        healing: None,
        guardian: None,
        outcome: None,
        stages: Vec::new(),
    }
}
