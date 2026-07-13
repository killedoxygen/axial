use super::*;
use crate::state::{
    AppState, AppStateInit, InstallStore, ProducerLease, RequestLease, SessionStore,
};
use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
use axial_launcher::{LaunchSessionRecord, LaunchState, SessionId};
use axial_minecraft::VersionEntry;
use axial_performance::PerformanceManager;
use axum::http::{HeaderValue, header};
use sha1::{Digest as _, Sha1};
use std::{
    collections::HashMap,
    fs, io,
    path::Path as FsPath,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

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
            "failed to initialize instance files: /home/zero/.config/Axial/instances/new/logs",
            "Could not create the instance. Check app data permissions and try again.",
        ),
        (
            InstanceWriteOperation::Update,
            "failed to persist /home/zero/.config/Axial/instances.json",
            "Could not save the instance. Check app data permissions and try again.",
        ),
        (
            InstanceWriteOperation::Delete,
            "failed to delete C:\\Users\\Zero\\AppData\\Roaming\\Axial\\instances\\old",
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
        "/home/zero/.config/Axial/instances/source/mods/example.jar; ",
        "failed to roll back persisted instance: ",
        "C:\\Users\\Zero\\AppData\\Roaming\\Axial\\config\\instances.json"
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
        "metadata failed for /home/zero/.config/Axial/instances/test/logs/latest.log",
        "open failed for C:\\Users\\Zero\\AppData\\Roaming\\Axial\\instances\\test\\logs\\debug.log",
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
    let game_dir = FsPath::new("/tmp/axial-instance");

    assert_eq!(
        resolve_instance_folder(game_dir, None).expect("resolve root"),
        game_dir
    );
}

#[test]
fn instance_folder_resolver_accepts_allowed_subfolder() {
    let game_dir = FsPath::new("/tmp/axial-instance");

    assert_eq!(
        resolve_instance_folder(game_dir, Some("mods")).expect("resolve mods"),
        game_dir.join("mods")
    );
}

#[test]
fn instance_folder_resolver_rejects_unknown_subfolder() {
    let game_dir = FsPath::new("/tmp/axial-instance");

    assert_eq!(
        resolve_instance_folder(game_dir, Some("versions")),
        Err("invalid instance folder")
    );
}

#[test]
fn instance_folder_resolver_rejects_traversal_like_subfolders() {
    let game_dir = FsPath::new("/tmp/axial-instance");

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
        "axial-api-instance-logs-{}-{}",
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
        .insert_for_test("Tail invalid log".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Tail truncated log".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Redacted log tail".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Serve screenshots".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Large screenshot".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Rename screenshots".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Delete screenshots".to_string(), "1.21.1".to_string())
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
            "failed for /home/zero/.config/Axial/instances/test/screenshots/shot.png",
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
        .insert_for_test("Update mods".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Delete mods".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Rename worlds".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Delete worlds".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Backup worlds".to_string(), "1.21.1".to_string())
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
        .insert_for_test("Running worlds".to_string(), "1.21.1".to_string())
        .expect("add instance");
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    fs::create_dir_all(game_dir.join("saves").join("World")).expect("create world");
    fixture
        .state
        .sessions()
        .insert(test_launch_record("active-world-session", &instance.id))
        .await
        .expect("insert session");

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
        .insert_for_test("Alpha".to_string(), "1.21.1".to_string())
        .expect("add alpha");
    let beta = fixture
        .state
        .instances()
        .insert_for_test("Beta".to_string(), "1.21.1".to_string())
        .expect("add beta");

    let updated = handle_update_instance(
        &fixture.state,
        &fixture.producer,
        &alpha.id,
        InstancePatch {
            name: Some(alpha.name.clone()),
            max_memory_mb: Some(3072),
            ..InstancePatch::default()
        },
    )
    .await
    .expect("unchanged name update should succeed");
    assert_eq!(updated.name, "Alpha");
    assert_eq!(updated.version_id, "1.21.1");
    assert_eq!(updated.max_memory_mb, 3072);

    let (status, Json(body)) = handle_update_instance(
        &fixture.state,
        &fixture.producer,
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
        .insert_for_test("Preset tamper".to_string(), "1.21.1".to_string())
        .expect("add instance");

    let updated = handle_update_instance(
        &fixture.state,
        &fixture.producer,
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
        .insert_for_test("Runtime override".to_string(), "1.21.1".to_string())
        .expect("add instance");

    let raw_java = r"C:\Users\Alice\.jdks\bad\bin\java.exe";
    let raw_args = "-Dtoken=raw-secret-token -javaagent:C:\\Users\\Alice\\agent.jar";
    let updated = handle_update_instance(
        &fixture.state,
        &fixture.producer,
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
async fn public_instance_responses_redact_stored_runtime_overrides() {
    let fixture = TestFixture::new("instance-runtime-overrides-redacted");
    let mut instance = fixture
        .state
        .instances()
        .insert_for_test("Runtime override".to_string(), "1.21.1".to_string())
        .expect("add instance");
    let raw_java = r"C:\Users\Alice\.jdks\bad\bin\java.exe";
    let raw_args = "-Dtoken=raw-secret-token -javaagent:C:\\Users\\Alice\\agent.jar";
    instance.java_path = raw_java.to_string();
    instance.extra_jvm_args = raw_args.to_string();
    fixture
        .state
        .instances()
        .replace_for_test(instance.clone())
        .expect("store runtime overrides");

    let listed = handle_list_instances(&fixture.state, &fixture.producer).await;
    let fetched = handle_get_instance(&fixture.state, &fixture.producer, &instance.id)
        .await
        .expect("get instance");
    let duplicated =
        handle_duplicate_instance(&fixture.state, &fixture.producer, &instance.id, None)
            .await
            .expect("duplicate instance");

    assert_eq!(listed.instances[0].instance.java_path, "");
    assert_eq!(listed.instances[0].instance.extra_jvm_args, "");
    assert_eq!(fetched.instance.java_path, "");
    assert_eq!(fetched.instance.extra_jvm_args, "");
    assert_eq!(duplicated.instance.java_path, "");
    assert_eq!(duplicated.instance.extra_jvm_args, "");

    let stored_duplicate = fixture
        .state
        .instances()
        .get(&duplicated.id)
        .expect("stored duplicate");
    assert_eq!(stored_duplicate.java_path, raw_java);
    assert_eq!(stored_duplicate.extra_jvm_args, raw_args);

    for public in [
        serde_json::to_string(&listed).expect("serialize list"),
        serde_json::to_string(&fetched).expect("serialize get"),
        serde_json::to_string(&duplicated).expect("serialize duplicate"),
    ] {
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
                "{leaked:?} leaked in instance response: {public}"
            );
        }
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
    assert_eq!(created.instance.name, "Survival");
    assert_eq!(created.instance.version_id, "1.21.1");
    assert_eq!(created.instance.icon, "grass");
    assert_eq!(created.instance.accent, "#5aa469");

    let listed = handle_list_instances(&fixture.state, &fixture.producer).await;
    assert_eq!(listed.last_instance_id, None);
    assert_eq!(listed.instances.len(), 1);
    assert_eq!(listed.instances[0].instance.id, created.instance.id);
    assert_eq!(listed.instances[0].instance.name, "Survival");
    assert!(!listed.instances[0].launchable);
    assert_eq!(
        listed.instances[0].status_detail,
        "Installed version metadata is missing. Install this version before launching."
    );
    assert_eq!(listed.instances[0].launch_action.label, "Install");
    assert_eq!(
        listed.instances[0].launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Install
    );

    let fetched = handle_get_instance(&fixture.state, &fixture.producer, &created.instance.id)
        .await
        .expect("get instance");
    assert_eq!(fetched.instance, created.instance.instance);

    let updated = handle_update_instance(
        &fixture.state,
        &fixture.producer,
        &created.instance.id,
        InstancePatch {
            name: Some("Skyblock".to_string()),
            max_memory_mb: Some(4096),
            icon: Some("cloud".to_string()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect("update instance");
    assert_eq!(updated.id, created.instance.id);
    assert_eq!(updated.name, "Skyblock");
    assert_eq!(updated.version_id, "1.21.1");
    assert_eq!(updated.max_memory_mb, 4096);
    assert_eq!(updated.icon, "cloud");

    let game_dir = fixture.state.instances().game_dir(&created.instance.id);
    fs::write(game_dir.join("logs").join("latest.log"), "started").expect("write log");

    let body = handle_delete_instance(&fixture.state, &created.instance.id, HashMap::new())
        .await
        .expect("delete instance");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(
        fixture
            .state
            .instances()
            .get(&created.instance.id)
            .is_none()
    );
    assert!(!game_dir.exists());
}

#[tokio::test]
async fn list_instances_summary_reports_missing_libraries() {
    let fixture = TestFixture::new("list-summary-missing-libraries");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_with_missing_library(&library_dir, "1.21.1");
    let instance = add_test_instance(&fixture, "Missing library", "1.21.1");

    let listed = listed_instance(&fixture, &instance.id).await;

    assert_not_launch_action(&listed);
    assert_eq!(listed.launch_action.state_id, "install_required");
    assert_eq!(
        listed.launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Install
    );
    assert_eq!(
        listed.status_detail,
        "Required libraries are missing. Install this version before launching."
    );
}

#[tokio::test]
async fn list_instances_summary_does_not_walk_asset_objects() {
    let fixture = TestFixture::new("list-summary-skips-asset-objects");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_with_missing_asset_object(&library_dir, "1.21.1");
    let instance = add_test_instance(&fixture, "Missing asset", "1.21.1");

    let listed = listed_instance(&fixture, &instance.id).await;

    assert!(listed.launchable);
    assert_eq!(listed.launch_action.state_id, "launch_ready");
    assert_eq!(
        listed.launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Launch
    );
}

#[tokio::test]
async fn list_instances_summary_does_not_hash_client_jar() {
    let fixture = TestFixture::new("list-summary-skips-client-hash");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_with_corrupt_client_jar(&library_dir, "1.21.1");
    let instance = add_test_instance(&fixture, "Corrupt client", "1.21.1");

    let listed = listed_instance(&fixture, &instance.id).await;

    assert!(listed.launchable);
    assert_eq!(listed.launch_action.state_id, "launch_ready");
    assert_eq!(
        listed.launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Launch
    );
}

#[tokio::test]
async fn list_instances_incomplete_parent_marker_does_not_show_launch_action() {
    let fixture = TestFixture::new("list-readiness-incomplete-parent");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_child_version_with_incomplete_parent(
        &library_dir,
        "fabric-loader-0.16.14-1.21.1",
        "1.21.1",
    );
    let instance = add_test_instance(
        &fixture,
        "Incomplete parent",
        "fabric-loader-0.16.14-1.21.1",
    );

    let listed = listed_instance(&fixture, &instance.id).await;

    assert_not_launch_action(&listed);
    assert_eq!(
        listed.launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Install
    );
    assert!(listed.status_detail.contains("incomplete"));
}

#[tokio::test]
async fn list_instances_installed_ready_version_transitions_to_launch_action() {
    let fixture = TestFixture::new("list-readiness-ready-launch");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_installed_vanilla_version(&library_dir, "1.21.1");
    let instance = add_test_instance(&fixture, "Ready", "1.21.1");

    let listed = listed_instance(&fixture, &instance.id).await;

    assert!(listed.launchable);
    assert_eq!(listed.launch_action.state_id, "launch_ready");
    assert_eq!(
        listed.launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Launch
    );
}

#[tokio::test]
async fn degraded_version_scan_blocks_instances_and_create_queue_checks() {
    let fixture = TestFixture::new("degraded-version-scan");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["1.21.1"]);
    let bad_version_dir = library_dir.join("versions").join("1.21.1");
    fs::create_dir_all(&bad_version_dir).expect("create bad version dir");
    fs::write(bad_version_dir.join("1.21.1.json"), "{not valid json")
        .expect("write malformed version json");
    add_test_instance(&fixture, "Malformed", "1.21.1");

    let listed = handle_list_instances(&fixture.state, &fixture.producer).await;

    assert!(listed.scan_state.degraded);
    assert_eq!(listed.scan_state.state_id, "degraded");
    assert_eq!(listed.instances.len(), 1);
    assert_eq!(
        listed.instances[0].launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Blocked
    );
    assert_eq!(
        listed.instances[0].status_detail,
        crate::application::version::VERSION_SCAN_DEGRADED_MESSAGE
    );

    let create_view = handle_create_instance_view(&fixture.state, &fixture.producer, None).await;
    assert!(create_view.versions.is_empty());
    assert!(create_view.notices.iter().any(|notice| {
        notice.state_id == "library_scan_degraded"
            && notice.detail.as_deref()
                == Some(crate::application::version::VERSION_SCAN_DEGRADED_MESSAGE)
    }));

    let (status, Json(body)) = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Blocked create".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect_err("degraded scan should block create queue checks");
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_bounded_error_body(
        &body,
        crate::application::version::VERSION_SCAN_DEGRADED_MESSAGE,
    );
    assert_eq!(fixture.state.instances().list().len(), 1);
}

#[tokio::test]
async fn update_instance_rejects_raw_bad_version_id_change() {
    let fixture = TestFixture::new("update-rejects-raw-version");
    let instance = add_test_instance(&fixture, "Stable", "1.21.1");

    let (status, Json(body)) = handle_update_instance(
        &fixture.state,
        &fixture.producer,
        &instance.id,
        InstancePatch {
            version_id: Some("bad-version".to_string()),
            ..InstancePatch::default()
        },
    )
    .await
    .expect_err("raw version retarget should fail");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_bounded_error_body(&body, "direct version changes are not supported");
    assert_eq!(
        fixture
            .state
            .instances()
            .get(&instance.id)
            .expect("instance remains")
            .version_id,
        "1.21.1"
    );
}

#[tokio::test]
async fn create_ready_instance_rebuilds_known_good_once() {
    let fixture = TestFixture::new("create-ready-known-good");
    let library_dir = fixture.configure_create_manifest(&["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    let rebuilds = Arc::new(AtomicUsize::new(0));
    let observed_rebuilds = rebuilds.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let create_state = fixture.state.clone();
    let create = tokio::spawn(async move {
        super::create::handle_create_instance_with_rebuild(
            &create_state,
            CreateInstanceRequest {
                name: "Ready".to_string(),
                selection_id: "vanilla|1.21.1".to_string(),
                ..CreateInstanceRequest::default()
            },
            move |_, _| async move {
                observed_rebuilds.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).expect("signal rebuild entry");
                release_rx.await.expect("release rebuild");
                Ok(())
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("rebuild enters")
        .expect("rebuild entry signal");
    assert!(!create.is_finished(), "create must wait for rebuild");
    release_tx.send(()).expect("release rebuild");
    let created = tokio::time::timeout(std::time::Duration::from_secs(5), create)
        .await
        .expect("create completes")
        .expect("create task")
        .expect("create ready instance");

    assert!(created.install_queue.is_none());
    assert_eq!(rebuilds.load(Ordering::SeqCst), 1);
}

async fn seed_committed_busy_install(state: &AppState, queue_id: &str) {
    let install_id = format!("{queue_id}-install");
    state.installs().insert(install_id.clone()).await;
    state
        .installs()
        .enqueue_queued_install(
            queue_id.to_string(),
            crate::state::InstallQueueSpec::vanilla("busy".to_string()),
            crate::state::InstallQueuePlacement::Back,
        )
        .await;
    let reserved = state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("reserve committed busy install");
    assert_eq!(reserved.queue_id, queue_id);
    assert!(
        state
            .installs()
            .mark_queued_install_started(queue_id, install_id)
            .await
    );
}

#[tokio::test]
async fn create_queued_instance_does_not_rebuild_known_good() {
    let fixture = TestFixture::new("create-queued-no-known-good");
    fixture.configure_create_manifest(&["1.21.2"]);
    seed_committed_busy_install(&fixture.state, "busy-create-queue").await;
    let rebuilds = Arc::new(AtomicUsize::new(0));
    let observed_rebuilds = rebuilds.clone();

    let created = super::create::handle_create_instance_with_rebuild(
        &fixture.state,
        CreateInstanceRequest {
            name: "Queued".to_string(),
            selection_id: "vanilla|1.21.2".to_string(),
            ..CreateInstanceRequest::default()
        },
        move |_, _| async move {
            observed_rebuilds.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .expect("create queued instance");

    assert!(created.queued_install.is_some());
    assert_eq!(rebuilds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn create_missed_active_install_receipt_rolls_back_new_instance() {
    let fixture = TestFixture::new("create-missed-active-receipt");
    fixture.configure_create_manifest(&["1.21.2"]);
    fixture
        .state
        .installs()
        .enqueue_queued_install(
            "active-selected-install".to_string(),
            crate::state::InstallQueueSpec::vanilla("1.21.2".to_string()),
            crate::state::InstallQueuePlacement::Back,
        )
        .await;
    let active = fixture
        .state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("reserve selected active install");
    assert_eq!(active.queue_id, "active-selected-install");

    let (status, Json(body)) = super::create::handle_create_instance_with_rebuild(
        &fixture.state,
        CreateInstanceRequest {
            name: "Missed active receipt".to_string(),
            selection_id: "vanilla|1.21.2".to_string(),
            ..CreateInstanceRequest::default()
        },
        |_, _| async { panic!("queued create must not reconstruct") },
    )
    .await
    .expect_err("instance absent from active receipt fanout must roll back");

    assert_eq!(status, StatusCode::CONFLICT);
    assert_bounded_error_body(
        &body,
        "The active version install did not include this instance. Try again after it finishes.",
    );
    assert!(fixture.state.instances().list().is_empty());
    let queue = fixture.state.installs().queue_snapshot().await;
    assert_eq!(
        queue.active.as_ref().map(|entry| entry.queue_id.as_str()),
        Some("active-selected-install")
    );
}

#[tokio::test]
async fn create_rebuild_failure_rolls_back_the_new_instance() {
    let fixture = TestFixture::new("create-known-good-rollback");
    let library_dir = fixture.configure_create_manifest(&["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    let created_id = Arc::new(Mutex::new(None::<String>));
    let observed_id = created_id.clone();

    let (status, Json(body)) = super::create::handle_create_instance_with_rebuild(
        &fixture.state,
        CreateInstanceRequest {
            name: "Rollback".to_string(),
            selection_id: "vanilla|1.21.1".to_string(),
            ..CreateInstanceRequest::default()
        },
        move |_, instance_id| async move {
            *observed_id.lock().expect("capture created id") = Some(instance_id);
            Err(crate::state::KnownGoodRebuildError::ReconstructionFailed)
        },
    )
    .await
    .expect_err("failed rebuild rolls back create");

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_bounded_error_body(
        &body,
        "Could not verify the selected version. Check your connection and try again.",
    );
    let created_id = created_id
        .lock()
        .expect("read created id")
        .clone()
        .expect("rebuild observed created id");
    assert!(fixture.state.instances().get(&created_id).is_none());
    assert!(!fixture.state.instances().game_dir(&created_id).exists());
}

#[tokio::test]
async fn duplicate_instance_rebuilds_known_good_once() {
    let fixture = TestFixture::new("duplicate-known-good");
    let source = add_test_instance(&fixture, "Source", "1.21.1");
    let rebuilds = Arc::new(AtomicUsize::new(0));
    let observed_rebuilds = rebuilds.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let duplicate_state = fixture.state.clone();
    let source_id = source.id.clone();
    let request = duplicate_state
        .try_admit_request()
        .expect("admit duplicate request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim duplicate producer");
    let duplicate = tokio::spawn(async move {
        let _request = request;
        handle_duplicate_instance_with_rebuild(
            &duplicate_state,
            &producer,
            &source_id,
            None,
            move |_, _| async move {
                observed_rebuilds.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).expect("signal rebuild entry");
                release_rx.await.expect("release rebuild");
                Ok(())
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("rebuild enters")
        .expect("rebuild entry signal");
    assert!(!duplicate.is_finished(), "duplicate must wait for rebuild");
    release_tx.send(()).expect("release rebuild");
    let duplicate = tokio::time::timeout(std::time::Duration::from_secs(5), duplicate)
        .await
        .expect("duplicate completes")
        .expect("duplicate task")
        .expect("duplicate instance");

    assert_ne!(duplicate.id, source.id);
    assert!(fixture.state.instances().get(&source.id).is_some());
    assert!(fixture.state.instances().get(&duplicate.id).is_some());
    assert_eq!(rebuilds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_rebuild_failure_rolls_back_only_the_copy() {
    let fixture = TestFixture::new("duplicate-known-good-rollback");
    let source = add_test_instance(&fixture, "Source", "1.21.1");
    let duplicate_id = Arc::new(Mutex::new(None::<String>));
    let observed_id = duplicate_id.clone();

    let (status, Json(body)) = handle_duplicate_instance_with_rebuild(
        &fixture.state,
        &fixture.producer,
        &source.id,
        None,
        move |_, instance_id| async move {
            *observed_id.lock().expect("capture duplicate id") = Some(instance_id);
            Err(crate::state::KnownGoodRebuildError::ReconstructionFailed)
        },
    )
    .await
    .expect_err("failed rebuild rolls back duplicate");

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_bounded_error_body(
        &body,
        "Could not verify the selected version. Check your connection and try again.",
    );
    let duplicate_id = duplicate_id
        .lock()
        .expect("read duplicate id")
        .clone()
        .expect("rebuild observed duplicate id");
    assert!(fixture.state.instances().get(&source.id).is_some());
    assert!(fixture.state.instances().get(&duplicate_id).is_none());
    assert!(!fixture.state.instances().game_dir(&duplicate_id).exists());
}

#[tokio::test]
async fn dropped_create_caller_keeps_rebuild_rollback_owned_until_quiescence() {
    let (state, root) = test_state("create-known-good-caller-drop");
    let library_dir = root.join("library");
    state.set_library_dir_for_test(library_dir.to_string_lossy().into_owned());
    write_version_manifest_cache(&library_dir, &["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    let created_id = Arc::new(Mutex::new(None::<String>));
    let observed_id = created_id.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let create_state = state.clone();
    let caller = tokio::spawn(async move {
        super::create::handle_create_instance_with_rebuild(
            &create_state,
            CreateInstanceRequest {
                name: "Owned rollback".to_string(),
                selection_id: "vanilla|1.21.1".to_string(),
                ..CreateInstanceRequest::default()
            },
            move |_, instance_id| async move {
                *observed_id.lock().expect("capture created id") = Some(instance_id);
                entered_tx.send(()).expect("signal rebuild entry");
                release_rx.await.expect("release rebuild");
                Err(crate::state::KnownGoodRebuildError::ReconstructionFailed)
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("rebuild enters")
        .expect("rebuild entry signal");
    let created_id = created_id
        .lock()
        .expect("read created id")
        .clone()
        .expect("rebuild observed created id");
    assert!(state.instances().get(&created_id).is_some());
    caller.abort();
    assert!(
        caller
            .await
            .expect_err("caller cancellation")
            .is_cancelled()
    );

    let shutdown_state = state.clone();
    let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.lifecycle_phase() != crate::state::AppLifecyclePhase::QuiescingProducers {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("quiescence waits on accepted create");
    assert!(!quiesce.is_finished());
    release_tx.send(()).expect("release failed rebuild");
    tokio::time::timeout(std::time::Duration::from_secs(5), quiesce)
        .await
        .expect("rollback drains")
        .expect("quiesce task")
        .expect("quiesce succeeds");

    assert!(state.instances().get(&created_id).is_none());
    assert!(!state.instances().game_dir(&created_id).exists());
    state
        .close_known_good_inventories()
        .await
        .expect("close known-good store");
    state
        .close_instance_registry()
        .await
        .expect("close instance registry");
    drop(state);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn create_instance_duplicate_name_gets_backend_owned_suffix() {
    let fixture = TestFixture::new("create-name-conflict");
    let library_dir = fixture.configure_create_manifest(&["1.21.1", "1.21.2"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    write_installed_vanilla_version(&library_dir, "1.21.2");
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
    assert_eq!(original.instance.name, "Survival");

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

    assert_eq!(created.instance.name, "Survival (1)");
    assert_eq!(created.instance.version_id, "1.21.2");
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
    assert_eq!(created.instance.max_memory_mb, 6144);
    assert_eq!(created.instance.min_memory_mb, 1024);
    assert_eq!(created.instance.window_width, 1280);
    assert_eq!(created.instance.window_height, 720);
    assert_eq!(created.instance.art_seed, 42);
    assert_eq!(created.instance.jvm_preset, "performance");
    assert!(created.guardian_notice.is_none());
    assert_eq!(
        fixture
            .state
            .instances()
            .get(&created.instance.id)
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

    assert_eq!(created.instance.jvm_preset, "");
    assert_eq!(
        created
            .guardian_notice
            .as_ref()
            .expect("guardian notice")
            .state_id(),
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

    assert_eq!(created.instance.jvm_preset, "");
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
async fn create_instance_rejects_unknown_exact_loader_selection() {
    let fixture = TestFixture::new("create-unknown-loader-build-selection");
    let library_dir = fixture.configure_create_manifest(&["1.21.1"]);
    write_fabric_loader_build_cache(&library_dir, "1.21.1", "0.16.14");
    let unknown_build_id = axial_minecraft::build_id_for(
        axial_minecraft::LoaderComponentId::Fabric,
        "1.21.1",
        "0.16.99",
    );

    let (status, Json(body)) = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Unknown loader".to_string(),
            selection_id: format!(
                "loader_build|{}|{unknown_build_id}",
                axial_minecraft::LoaderComponentId::Fabric.as_str()
            ),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect_err("unknown exact loader build should fail");

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "Selected loader build is not available.");
    assert!(fixture.state.instances().list().is_empty());
}

#[test]
fn loader_create_selection_rejects_stale_uninstalled_exact_build() {
    let component_id = axial_minecraft::LoaderComponentId::Fabric;
    let build = fabric_build_record(component_id, "1.21.1", "0.16.14", 900);
    let build_id = build.build_id.clone();

    let (status, Json(body)) = resolve_loader_create_selection_from_build_catalog(
        component_id,
        &build_id,
        vec![build],
        &loader_catalog_state(false, true),
        &[],
    )
    .expect_err("stale uninstalled exact selection should fail");

    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_bounded_error_body(
        &body,
        "Loader catalog needs a fresh provider check before this build can be installed.",
    );
}

#[test]
fn loader_create_selection_allows_stale_exact_build_when_already_installed() {
    let component_id = axial_minecraft::LoaderComponentId::Fabric;
    let build = fabric_build_record(component_id, "1.21.1", "0.16.14", 900);
    let installed_version = installed_loader_entry(&build);

    let selection = resolve_loader_create_selection_from_build_catalog(
        component_id,
        &build.build_id,
        vec![build.clone()],
        &loader_catalog_state(false, true),
        &[installed_version],
    )
    .expect("stale exact installed selection can be reused");

    assert_eq!(
        selection,
        CreateSelection::Loader {
            component_id,
            build_id: build.build_id,
            target_version_id: build.version_id
        }
    );
}

#[tokio::test]
async fn create_instance_loader_version_uses_beta_build_when_only_beta_builds_exist() {
    let fixture = TestFixture::new("create-loader-version-beta-only");
    let library_dir = fixture.configure_create_manifest(&["26.2"]);
    let component_id = axial_minecraft::LoaderComponentId::NeoForge;
    let mut beta = fabric_build_record(component_id, "26.2", "26.2.0.3-beta", 600);
    beta.build_meta.selection.reason = axial_minecraft::LoaderSelectionReason::Unstable;
    beta.build_meta.selection.source = axial_minecraft::LoaderSelectionSource::ExplicitVersionLabel;
    let beta_version_id = beta.version_id.clone();
    write_loader_build_cache_records(&library_dir, component_id, "26.2", vec![beta]);
    seed_committed_busy_install(&fixture.state, "busy-beta-queue").await;

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "NeoForge beta default".to_string(),
            selection_id: "loader_version|net.neoforged|26.2".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("beta-only loader version should create");

    assert_eq!(created.instance.version_id, beta_version_id);
    let queued = created.queued_install.expect("queued install summary");
    assert_eq!(queued.kind, "loader");
    assert_eq!(queued.state_id, "install_queued");
    assert_eq!(queued.label, "NeoForge 26.2.0.3-beta for Minecraft 26.2");
}

#[tokio::test]
async fn create_instance_quilt_java25_default_uses_compatible_beta_fallback() {
    let fixture = TestFixture::new("create-quilt-java25-beta-fallback");
    let library_dir = fixture.configure_create_manifest(&["26.1.2"]);
    let component_id = axial_minecraft::LoaderComponentId::Quilt;
    let mut stable_build = fabric_build_record(component_id, "26.1.2", "0.29.2", 700);
    stable_build.build_meta.selection.reason = axial_minecraft::LoaderSelectionReason::Unlabeled;
    stable_build.build_meta.selection.source = axial_minecraft::LoaderSelectionSource::None;
    let mut beta_build = fabric_build_record(component_id, "26.1.2", "0.30.0-beta.8", 600);
    beta_build.build_meta.selection.reason = axial_minecraft::LoaderSelectionReason::Unstable;
    beta_build.build_meta.selection.source =
        axial_minecraft::LoaderSelectionSource::ExplicitVersionLabel;
    write_loader_build_cache_records(
        &library_dir,
        component_id,
        "26.1.2",
        vec![stable_build, beta_build],
    );
    seed_committed_busy_install(&fixture.state, "busy-quilt-beta-queue").await;

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Quilt 26".to_string(),
            selection_id: "loader_version|org.quiltmc.quilt-loader|26.1.2".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("compatible Quilt beta fallback should create");

    assert_eq!(
        created.instance.version_id,
        axial_minecraft::installed_version_id_for(
            axial_minecraft::LoaderComponentId::Quilt,
            "26.1.2",
            "0.30.0-beta.8",
        )
        .expect("valid Quilt identity")
    );
    let queued = created.queued_install.expect("queued install summary");
    assert_eq!(queued.kind, "loader");
    assert_eq!(queued.label, "Quilt 0.30.0-beta.8 for Minecraft 26.1.2");
}

#[tokio::test]
async fn create_instance_view_returns_backend_authored_version_rows() {
    let fixture = TestFixture::new("create-view-version-rows");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["1.21.1", "1.21.2"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    for component in axial_minecraft::fetch_components() {
        let versions = if component.id == axial_minecraft::LoaderComponentId::Fabric {
            vec!["1.21.1"]
        } else {
            Vec::new()
        };
        write_supported_versions_cache(&library_dir, component.id, &versions);
    }
    write_fabric_loader_build_cache(&library_dir, "1.21.1", "0.16.14");

    let view = handle_create_instance_view(&fixture.state, &fixture.producer, None).await;

    let vanilla = view
        .versions
        .iter()
        .find(|row| row.source_id == "vanilla" && row.minecraft_version_id == "1.21.1")
        .expect("vanilla row");
    assert_eq!(vanilla.selection_id, "vanilla|1.21.1");
    assert_eq!(vanilla.download_state, "full");
    assert_eq!(vanilla.channel, "release");
    assert!(
        view.versions.iter().all(|row| row.source_id == "vanilla"),
        "default create-view should stay vanilla-only"
    );

    let view = handle_create_instance_view(
        &fixture.state,
        &fixture.producer,
        Some(axial_minecraft::LoaderComponentId::Fabric.as_str()),
    )
    .await;
    let fabric = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == axial_minecraft::LoaderComponentId::Fabric.as_str()
                && row.minecraft_version_id == "1.21.1"
        })
        .expect("fabric row");
    assert_eq!(
        fabric.selection_id,
        "loader_version|net.fabricmc.fabric-loader|1.21.1"
    );
    assert!(fabric.loader_build.is_none());
    assert_eq!(fabric.download_state, "base");
    assert!(
        view.notices.is_empty(),
        "unexpected notices: {:?}",
        view.notices
    );
}

#[tokio::test]
async fn create_instance_view_marks_loader_minecraft_row_full_when_any_loader_is_installed() {
    let fixture = TestFixture::new("create-view-exact-loader-installed");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    for component in axial_minecraft::fetch_components() {
        let versions = if component.id == axial_minecraft::LoaderComponentId::Fabric {
            vec!["1.21.1"]
        } else {
            Vec::new()
        };
        write_supported_versions_cache(&library_dir, component.id, &versions);
    }
    write_fabric_loader_build_cache_with_builds(
        &library_dir,
        "1.21.1",
        &[("0.16.15", 900), ("0.16.14", 900)],
        chrono::Utc::now().timestamp_millis(),
    );
    write_installed_loader_version(
        &library_dir,
        axial_minecraft::LoaderComponentId::Fabric,
        "1.21.1",
        "0.16.14",
    );

    let view = handle_create_instance_view(
        &fixture.state,
        &fixture.producer,
        Some(axial_minecraft::LoaderComponentId::Fabric.as_str()),
    )
    .await;

    let fabric = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == axial_minecraft::LoaderComponentId::Fabric.as_str()
                && row.minecraft_version_id == "1.21.1"
        })
        .expect("fabric row");
    assert_eq!(
        fabric.selection_id,
        "loader_version|net.fabricmc.fabric-loader|1.21.1"
    );
    assert_eq!(fabric.download_state, "full");
}

#[tokio::test]
async fn create_instance_view_refreshes_when_versions_root_metadata_changes() {
    super::create_cache::reset_create_view_cache_for_tests();
    let fixture = TestFixture::new("create-view-installed-scan-cache");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["1.21.1"]);

    let view = handle_create_instance_view(&fixture.state, &fixture.producer, None).await;
    let row = view
        .versions
        .iter()
        .find(|row| row.source_id == "vanilla" && row.minecraft_version_id == "1.21.1")
        .expect("vanilla row");
    assert_eq!(row.download_state, "none");
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);

    write_installed_vanilla_version(&library_dir, "1.21.1");
    let refreshed_view = handle_create_instance_view(&fixture.state, &fixture.producer, None).await;
    let refreshed_row = refreshed_view
        .versions
        .iter()
        .find(|row| row.source_id == "vanilla" && row.minecraft_version_id == "1.21.1")
        .expect("refreshed vanilla row");
    assert_eq!(refreshed_row.download_state, "full");
    assert_eq!(fixture.state.installed_versions_walk_count(), 2);

    let cached_view = handle_create_instance_view(&fixture.state, &fixture.producer, None).await;
    let cached_row = cached_view
        .versions
        .iter()
        .find(|row| row.source_id == "vanilla" && row.minecraft_version_id == "1.21.1")
        .expect("cached vanilla row");
    assert_eq!(cached_row.download_state, "full");
    assert_eq!(fixture.state.installed_versions_walk_count(), 2);
}

#[tokio::test]
async fn create_instance_view_tags_beta_only_loader_version_rows_without_blocking_selection() {
    let fixture = TestFixture::new("create-view-beta-only-loader-version");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["26.2", "1.7.10_pre4"]);
    for component in axial_minecraft::fetch_components() {
        let versions = match component.id {
            axial_minecraft::LoaderComponentId::Forge => {
                vec![("1.7.10_pre4", Some(false))]
            }
            axial_minecraft::LoaderComponentId::NeoForge => vec![("26.2", Some(false))],
            _ => Vec::new(),
        };
        write_supported_versions_cache_with_stable_hints(&library_dir, component.id, &versions);
    }
    write_installed_vanilla_version(&library_dir, "26.2");
    write_installed_loader_version(
        &library_dir,
        axial_minecraft::LoaderComponentId::NeoForge,
        "26.2",
        "26.2.0.6-beta",
    );

    for (component_id, minecraft_version) in [
        (axial_minecraft::LoaderComponentId::Forge, "1.7.10_pre4"),
        (axial_minecraft::LoaderComponentId::NeoForge, "26.2"),
    ] {
        let view = handle_create_instance_view(
            &fixture.state,
            &fixture.producer,
            Some(component_id.as_str()),
        )
        .await;

        let row = view
            .versions
            .iter()
            .find(|row| {
                row.source_id == component_id.as_str()
                    && row.minecraft_version_id == minecraft_version
            })
            .expect("beta-only loader row");
        assert_eq!(row.channel, "snapshot");
        if component_id == axial_minecraft::LoaderComponentId::NeoForge {
            assert_eq!(row.download_state, "full");
        }
        assert!(row.create_enabled);
        assert_eq!(row.disabled_reason, None);
        assert!(
            row.tags
                .iter()
                .any(|tag| tag.id == "beta" && tag.label == "Beta")
        );
    }
}

#[tokio::test]
async fn create_instance_view_keeps_fabric_and_quilt_snapshot_rows_enabled() {
    let fixture = TestFixture::new("create-view-loader-snapshot-stable-hint");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["26.2"]);
    for component in axial_minecraft::fetch_components() {
        let versions = if matches!(
            component.id,
            axial_minecraft::LoaderComponentId::Fabric | axial_minecraft::LoaderComponentId::Quilt
        ) {
            vec![("26.2", Some(false))]
        } else {
            Vec::new()
        };
        write_supported_versions_cache_with_stable_hints(&library_dir, component.id, &versions);
    }
    write_loader_build_cache_records(
        &library_dir,
        axial_minecraft::LoaderComponentId::Quilt,
        "26.2",
        vec![fabric_build_record(
            axial_minecraft::LoaderComponentId::Quilt,
            "26.2",
            "0.30.0",
            700,
        )],
    );

    for component_id in [
        axial_minecraft::LoaderComponentId::Fabric,
        axial_minecraft::LoaderComponentId::Quilt,
    ] {
        let view = handle_create_instance_view(
            &fixture.state,
            &fixture.producer,
            Some(component_id.as_str()),
        )
        .await;
        let row = view
            .versions
            .iter()
            .find(|row| {
                row.source_id == component_id.as_str() && row.minecraft_version_id == "26.2"
            })
            .expect("snapshot loader row");
        assert_eq!(row.channel, "snapshot");
        assert!(row.create_enabled);
        assert_eq!(row.disabled_reason, None);
        assert!(row.tags.is_empty());
    }
}

#[tokio::test]
async fn create_instance_view_disables_known_incompatible_quilt_java25_default() {
    let fixture = TestFixture::new("create-view-quilt-java25-guard");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["26.1.3", "26.1.2", "1.21.10"]);
    for component in axial_minecraft::fetch_components() {
        let versions = if component.id == axial_minecraft::LoaderComponentId::Quilt {
            vec![
                ("26.1.3", Some(true)),
                ("26.1.2", Some(true)),
                ("1.21.10", Some(true)),
            ]
        } else {
            Vec::new()
        };
        write_supported_versions_cache_with_stable_hints(&library_dir, component.id, &versions);
    }
    let component_id = axial_minecraft::LoaderComponentId::Quilt;
    write_loader_build_cache_records(
        &library_dir,
        component_id,
        "26.1.2",
        vec![fabric_build_record(component_id, "26.1.2", "0.29.2", 700)],
    );
    write_loader_build_cache_records(
        &library_dir,
        component_id,
        "26.1.3",
        vec![fabric_build_record(component_id, "26.1.3", "0.30.0", 700)],
    );

    let view = handle_create_instance_view(
        &fixture.state,
        &fixture.producer,
        Some(axial_minecraft::LoaderComponentId::Quilt.as_str()),
    )
    .await;

    let quilt_26 = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == axial_minecraft::LoaderComponentId::Quilt.as_str()
                && row.minecraft_version_id == "26.1.2"
        })
        .expect("quilt 26 row");
    assert!(!quilt_26.create_enabled);
    assert_eq!(
        quilt_26.disabled_reason.as_deref(),
        Some("No stable compatible Quilt loader is available for this Minecraft version.")
    );
    let quilt_26_compatible = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == axial_minecraft::LoaderComponentId::Quilt.as_str()
                && row.minecraft_version_id == "26.1.3"
        })
        .expect("quilt compatible 26 row");
    assert!(quilt_26_compatible.create_enabled);

    let quilt_1_21 = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == axial_minecraft::LoaderComponentId::Quilt.as_str()
                && row.minecraft_version_id == "1.21.10"
        })
        .expect("quilt 1.21 row");
    assert!(quilt_1_21.create_enabled);
}

#[tokio::test]
async fn create_instance_view_tags_quilt_java25_without_cached_builds() {
    let fixture = TestFixture::new("create-view-quilt-java25-no-build-cache");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["26.1.2"]);
    for component in axial_minecraft::fetch_components() {
        let versions = if component.id == axial_minecraft::LoaderComponentId::Quilt {
            vec![("26.1.2", Some(true))]
        } else {
            Vec::new()
        };
        write_supported_versions_cache_with_stable_hints(&library_dir, component.id, &versions);
    }

    let view = handle_create_instance_view(
        &fixture.state,
        &fixture.producer,
        Some(axial_minecraft::LoaderComponentId::Quilt.as_str()),
    )
    .await;

    let row = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == axial_minecraft::LoaderComponentId::Quilt.as_str()
                && row.minecraft_version_id == "26.1.2"
        })
        .expect("quilt 26 row");
    assert!(row.create_enabled);
    assert_eq!(row.disabled_reason, None);
    assert!(row.tags.iter().any(|tag| tag.id == "beta"));
}

#[tokio::test]
async fn create_instance_view_enables_quilt_java25_when_compatible_beta_is_default_fallback() {
    let fixture = TestFixture::new("create-view-quilt-java25-beta-fallback");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    write_version_manifest_cache(&library_dir, &["26.1.2"]);
    for component in axial_minecraft::fetch_components() {
        let versions = if component.id == axial_minecraft::LoaderComponentId::Quilt {
            vec![("26.1.2", Some(true))]
        } else {
            Vec::new()
        };
        write_supported_versions_cache_with_stable_hints(&library_dir, component.id, &versions);
    }
    let component_id = axial_minecraft::LoaderComponentId::Quilt;
    let mut stable_build = fabric_build_record(component_id, "26.1.2", "0.29.2", 700);
    stable_build.build_meta.selection.reason = axial_minecraft::LoaderSelectionReason::Unlabeled;
    let mut beta_build = fabric_build_record(component_id, "26.1.2", "0.30.0-beta.8", 600);
    beta_build.build_meta.selection.reason = axial_minecraft::LoaderSelectionReason::Unstable;
    write_loader_build_cache_records(
        &library_dir,
        component_id,
        "26.1.2",
        vec![stable_build, beta_build],
    );

    let view = handle_create_instance_view(
        &fixture.state,
        &fixture.producer,
        Some(axial_minecraft::LoaderComponentId::Quilt.as_str()),
    )
    .await;

    let row = view
        .versions
        .iter()
        .find(|row| {
            row.source_id == axial_minecraft::LoaderComponentId::Quilt.as_str()
                && row.minecraft_version_id == "26.1.2"
        })
        .expect("quilt 26 row");
    assert!(row.create_enabled);
    assert_eq!(row.disabled_reason, None);
    assert!(row.tags.iter().any(|tag| tag.id == "beta"));
}

#[tokio::test]
async fn create_instance_vanilla_selection_returns_backend_queue_state() {
    let fixture = TestFixture::new("create-vanilla-queue");
    fixture
        .state
        .set_library_dir_for_test(fixture.root.join("library").to_string_lossy().to_string());
    write_version_manifest_cache(&fixture.root.join("library"), &["1.21.2"]);
    seed_committed_busy_install(&fixture.state, "busy-queue").await;
    fixture
        .state
        .installs()
        .enqueue_queued_install(
            "older-pending-queue".to_string(),
            crate::state::InstallQueueSpec::vanilla("1.20.1".to_string()),
            crate::state::InstallQueuePlacement::Back,
        )
        .await;

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

    assert_eq!(created.instance.version_id, "1.21.2");
    let queued = created.queued_install.expect("queued install summary");
    assert_eq!(queued.state_id, "install_queued");
    assert_eq!(queued.kind, "vanilla");
    assert_eq!(queued.label, "Minecraft 1.21.2");
    assert!(queued.queue_id.is_some());
    let install_queue = created.install_queue.expect("install queue");
    assert_eq!(
        install_queue.items.first().map(|item| item.label.as_str()),
        Some("Minecraft 1.20.1")
    );
    assert!(
        install_queue
            .items
            .iter()
            .any(|item| item.queue_id.as_str() == queued.queue_id.as_deref().unwrap_or_default())
    );
}

#[tokio::test]
async fn create_instance_installed_vanilla_selection_does_not_queue_install() {
    let fixture = TestFixture::new("create-installed-vanilla");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
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

    assert_eq!(created.instance.version_id, "1.21.1");
    assert!(created.instance.launchable);
    assert!(created.install_queue.is_none());
    assert!(created.queued_install.is_none());
    assert_eq!(created.view_model.state_id, "created");
}

#[tokio::test]
async fn create_instance_vanilla_reuses_one_request_snapshot_without_warm_walks() {
    let fixture = TestFixture::new("create-vanilla-shared-version-snapshot");
    let library_dir = fixture.configure_create_manifest(&["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");

    for name in ["Cold vanilla", "Warm vanilla"] {
        let created = handle_create_instance(
            &fixture.state,
            CreateInstanceRequest {
                name: name.to_string(),
                selection_id: "vanilla|1.21.1".to_string(),
                ..CreateInstanceRequest::default()
            },
        )
        .await
        .expect("create installed vanilla instance");

        assert!(created.instance.launchable);
        assert!(created.install_queue.is_none());
        assert_eq!(fixture.state.installed_versions_walk_count(), 1);
    }
}

#[tokio::test]
async fn create_instance_loader_reuses_one_request_snapshot_without_warm_walks() {
    let fixture = TestFixture::new("create-loader-shared-version-snapshot");
    let library_dir = fixture.configure_create_manifest(&["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    write_installed_loader_version(
        &library_dir,
        axial_minecraft::LoaderComponentId::Fabric,
        "1.21.1",
        "0.16.14",
    );
    let build_id = write_fabric_loader_build_cache(&library_dir, "1.21.1", "0.16.14");

    for name in ["Cold loader", "Warm loader"] {
        let created = handle_create_instance(
            &fixture.state,
            CreateInstanceRequest {
                name: name.to_string(),
                selection_id: format!(
                    "loader_build|{}|{build_id}",
                    axial_minecraft::LoaderComponentId::Fabric.as_str()
                ),
                ..CreateInstanceRequest::default()
            },
        )
        .await
        .expect("create installed loader instance");

        assert!(created.instance.launchable);
        assert!(created.install_queue.is_none());
        assert_eq!(fixture.state.installed_versions_walk_count(), 1);
    }
}

#[tokio::test]
async fn create_instance_checksumless_loader_probe_stays_strict_without_instance_authority() {
    let fixture = TestFixture::new("create-checksumless-loader-strict");
    let library_dir = fixture.configure_create_manifest(&["1.21.1"]);
    write_installed_vanilla_version(&library_dir, "1.21.1");
    write_installed_checksumless_loader_version(
        &library_dir,
        axial_minecraft::LoaderComponentId::Fabric,
        "1.21.1",
        "0.16.14",
    );
    let build_id = write_fabric_loader_build_cache(&library_dir, "1.21.1", "0.16.14");
    seed_committed_busy_install(&fixture.state, "busy-checksumless-probe").await;

    let created = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Strict checksumless loader".to_string(),
            selection_id: format!(
                "loader_build|{}|{build_id}",
                axial_minecraft::LoaderComponentId::Fabric.as_str()
            ),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect("create checksumless loader instance");

    assert!(!created.instance.launchable);
    assert!(created.queued_install.is_some());
}

#[tokio::test]
async fn cached_loader_build_cannot_authorize_backend_install() {
    let fixture = TestFixture::new("create-loader-queue");
    let library_dir = fixture.root.join("library");
    fixture
        .state
        .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    fixture
        .state
        .installs()
        .enqueue_queued_install(
            "busy-loader-queue".to_string(),
            crate::state::InstallQueueSpec::vanilla("busy".to_string()),
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

    let (status, Json(body)) = handle_create_instance(
        &fixture.state,
        CreateInstanceRequest {
            name: "Fabric queued".to_string(),
            selection_id: "loader_version|net.fabricmc.fabric-loader|1.21.99".to_string(),
            ..CreateInstanceRequest::default()
        },
    )
    .await
    .expect_err("cached build must not authorize a loader install");

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["failure_kind"], "catalog_unavailable");
    assert_eq!(
        body["error"],
        "Loader catalog is unavailable. Check your connection and try again."
    );
    assert_eq!(
        build_id,
        axial_minecraft::build_id_for(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.21.99",
            "0.16.14"
        )
    );
    assert!(fixture.state.instances().list().is_empty());
    let queue = fixture.state.installs().queue_snapshot().await;
    assert_eq!(
        queue.active.as_ref().map(|entry| entry.queue_id.as_str()),
        Some("busy-loader-queue")
    );
    assert!(queue.pending.is_empty());
}

#[tokio::test]
async fn duplicate_instance_existing_name_maps_to_conflict_json_error() {
    let fixture = TestFixture::new("duplicate-name-conflict");
    let source = fixture
        .state
        .instances()
        .insert_for_test("Source".to_string(), "1.21.1".to_string())
        .expect("add source instance");
    fixture
        .state
        .instances()
        .insert_for_test("Existing".to_string(), "1.21.1".to_string())
        .expect("add existing instance");

    let (status, Json(body)) = handle_duplicate_instance(
        &fixture.state,
        &fixture.producer,
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
        .insert_for_test("Traversal".to_string(), "1.21.1".to_string())
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

    let (status, Json(body)) = handle_get_instance(&fixture.state, &fixture.producer, "missing")
        .await
        .expect_err("missing get should fail");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_bounded_error_body(&body, "instance not found");

    let (status, Json(body)) = handle_update_instance(
        &fixture.state,
        &fixture.producer,
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
        .insert_for_test("Remove files".to_string(), "1.21.1".to_string())
        .expect("add remove-files instance");
    let remove_game_dir = fixture.state.instances().game_dir(&remove_files.id);
    fs::write(remove_game_dir.join("mods").join("example.jar"), "mod").expect("write mod");
    let known_good_dir = fixture.root.join("config").join("state").join("known-good");
    fs::create_dir_all(&known_good_dir).expect("create known-good cache directory");
    let remove_known_good = known_good_dir.join(format!("{}.json", remove_files.id));
    fs::write(&remove_known_good, "known-good").expect("write remove-files known-good cache");

    let body = handle_delete_instance(&fixture.state, &remove_files.id, HashMap::new())
        .await
        .expect("delete with default file removal");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(!remove_game_dir.exists());
    assert!(!remove_known_good.exists());

    let keep_files = fixture
        .state
        .instances()
        .insert_for_test("Keep files".to_string(), "1.21.1".to_string())
        .expect("add keep-files instance");
    let keep_game_dir = fixture.state.instances().game_dir(&keep_files.id);
    let keep_marker = keep_game_dir.join("saves").join("world").join("level.dat");
    fs::create_dir_all(keep_marker.parent().expect("marker parent")).expect("create world");
    fs::write(&keep_marker, "level").expect("write level");
    let keep_known_good = known_good_dir.join(format!("{}.json", keep_files.id));
    fs::write(&keep_known_good, "known-good").expect("write keep-files known-good cache");

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
    assert!(!keep_known_good.exists());
}

#[tokio::test]
async fn delete_waits_for_launch_admission_and_rejects_newly_queued_session() {
    let fixture = TestFixture::new("delete-launch-admission");
    let instance = add_test_instance(&fixture, "Launch admission", "1.21.1");
    let lifecycle = fixture.state.acquire_instance_lifecycle(&instance.id).await;
    let deleting_state = fixture.state.clone();
    let deleting_id = instance.id.clone();
    let mut delete = Box::pin(handle_delete_instance(
        &deleting_state,
        &deleting_id,
        HashMap::new(),
    ));
    {
        let waker = futures_util::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            std::future::Future::poll(delete.as_mut(), &mut context),
            std::task::Poll::Pending
        ));
    }

    fixture
        .state
        .sessions()
        .insert(test_launch_record("delete-launch-session", &instance.id))
        .await
        .expect("queue session while launch owns lifecycle admission");
    drop(lifecycle);

    let (status, Json(body)) = delete.await.expect_err("queued launch must block deletion");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_bounded_error_body(
        &body,
        "cannot delete a running instance; stop the game first",
    );
    assert!(fixture.state.instances().get(&instance.id).is_some());
    assert!(fixture.state.instances().game_dir(&instance.id).is_dir());
}

#[tokio::test]
async fn cancelled_delete_caller_cannot_cancel_lifecycle_waiting_owner() {
    let fixture = TestFixture::new("delete-cancel-lifecycle");
    let instance = add_test_instance(&fixture, "Cancel lifecycle", "1.21.1");
    let known_good = fixture
        .root
        .join("config/state/known-good")
        .join(format!("{}.json", instance.id));
    fs::create_dir_all(known_good.parent().expect("known-good parent")).expect("state directory");
    fs::write(&known_good, "known-good").expect("known-good snapshot");
    let lifecycle = fixture.state.acquire_instance_lifecycle(&instance.id).await;
    let state = fixture.state.clone();
    let instance_id = instance.id.clone();
    let mut delete =
        Box::pin(state.delete_instance(instance_id.clone(), false, fixture.producer.claim_child()));
    {
        let waker = futures_util::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            std::future::Future::poll(delete.as_mut(), &mut context),
            std::task::Poll::Pending
        ));
    }
    drop(delete);
    drop(lifecycle);

    wait_for_detached_delete(&fixture.state, &instance_id, Some(&known_good)).await;
}

#[tokio::test]
async fn cancelled_delete_caller_cannot_cancel_registry_waiting_owner() {
    let fixture = TestFixture::new("delete-cancel-registry");
    let instance = add_test_instance(&fixture, "Cancel registry", "1.21.1");
    let registry = fixture
        .state
        .instances()
        .acquire_mutation()
        .await
        .expect("hold registry mutation");
    let state = fixture.state.clone();
    let instance_id = instance.id.clone();
    let mut delete =
        Box::pin(state.delete_instance(instance_id.clone(), false, fixture.producer.claim_child()));
    {
        let waker = futures_util::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            std::future::Future::poll(delete.as_mut(), &mut context),
            std::task::Poll::Pending
        ));
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    drop(delete);
    drop(registry);

    wait_for_detached_delete(&fixture.state, &instance_id, None).await;
}

#[tokio::test]
async fn successful_registry_delete_without_absence_compensates_retirements() {
    let fixture = TestFixture::new("delete-postcondition");
    let instance = add_test_instance(&fixture, "Delete postcondition", "1.21.1");
    let known_good = fixture
        .root
        .join("config/state/known-good")
        .join(format!("{}.json", instance.id));
    fs::create_dir_all(known_good.parent().expect("known-good parent")).expect("state directory");
    fs::write(&known_good, "known-good").expect("known-good snapshot");
    drop(
        fixture
            .state
            .admit_managed_instance(&instance.id, false)
            .await
            .expect("create managed authority before deletion"),
    );
    fixture
        .state
        .instances()
        .succeed_next_delete_without_removal();

    let error = fixture
        .state
        .delete_instance(instance.id.clone(), false, fixture.producer.claim_child())
        .await
        .expect_err("registry presence must fail the deletion postcondition");
    let InstanceStoreError::Persistence(error) = error else {
        panic!("postcondition failure must be a persistence error");
    };
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(
        error
            .to_string()
            .contains("successful deletion without removing the instance")
    );
    assert!(fixture.state.instances().get(&instance.id).is_some());
    assert!(known_good.is_file());
    drop(
        fixture
            .state
            .admit_managed_instance(&instance.id, false)
            .await
            .expect("performance retirement must be compensated"),
    );
}

#[tokio::test]
async fn failed_registry_delete_with_presence_compensates_retirements() {
    let fixture = TestFixture::new("delete-registry-failure");
    let instance = add_test_instance(&fixture, "Delete failure", "1.21.1");
    let known_good = fixture
        .root
        .join("config/state/known-good")
        .join(format!("{}.json", instance.id));
    fs::create_dir_all(known_good.parent().expect("known-good parent")).expect("state directory");
    fs::write(&known_good, "known-good").expect("known-good snapshot");
    drop(
        fixture
            .state
            .admit_managed_instance(&instance.id, false)
            .await
            .expect("create managed authority before deletion"),
    );
    fixture.state.instances().fail_next_delete_without_removal();

    let error = fixture
        .state
        .delete_instance(instance.id.clone(), false, fixture.producer.claim_child())
        .await
        .expect_err("registry failure must fail deletion");
    let InstanceStoreError::Persistence(error) = error else {
        panic!("registry failure must remain a persistence error");
    };
    assert!(error.to_string().contains("injected instance registry"));
    assert!(fixture.state.instances().get(&instance.id).is_some());
    assert!(known_good.is_file());
    drop(
        fixture
            .state
            .admit_managed_instance(&instance.id, false)
            .await
            .expect("failed deletion must reopen managed authority"),
    );
}

#[tokio::test]
async fn deletion_recovers_latched_managed_state_before_registry_absence() {
    let fixture = TestFixture::new("delete-recover-latched");
    let instance = add_test_instance(&fixture, "Recover latch", "1.21.1");
    let known_good = fixture
        .root
        .join("config/state/known-good")
        .join(format!("{}.json", instance.id));
    fs::create_dir_all(known_good.parent().expect("known-good parent")).expect("state directory");
    fs::write(&known_good, "known-good").expect("known-good snapshot");
    let staged = latch_managed_instance(&fixture, &instance.id).await;
    fs::remove_file(staged).expect("make exact managed recovery possible");

    fixture
        .state
        .delete_instance(instance.id.clone(), false, fixture.producer.claim_child())
        .await
        .expect("recovered latch permits deletion");

    assert!(fixture.state.instances().get(&instance.id).is_none());
    assert!(!known_good.exists());
}

#[tokio::test]
async fn unrecoverable_latched_managed_state_preserves_present_instance_authorities() {
    let fixture = TestFixture::new("delete-unrecoverable-latch");
    let instance = add_test_instance(&fixture, "Retain latch", "1.21.1");
    let known_good = fixture
        .root
        .join("config/state/known-good")
        .join(format!("{}.json", instance.id));
    fs::create_dir_all(known_good.parent().expect("known-good parent")).expect("state directory");
    fs::write(&known_good, "known-good").expect("known-good snapshot");
    let staged = latch_managed_instance(&fixture, &instance.id).await;

    fixture
        .state
        .delete_instance(instance.id.clone(), false, fixture.producer.claim_child())
        .await
        .expect_err("unrecoverable latch must block deletion");

    assert!(fixture.state.instances().get(&instance.id).is_some());
    assert!(known_good.is_file());
    let error = fixture
        .state
        .admit_managed_instance(&instance.id, false)
        .await
        .err()
        .expect("managed identity remains latched");
    assert!(
        error
            .to_string()
            .contains("exact recovery could not prove a clean state")
    );
    fs::remove_file(staged).expect("repair managed recovery stage");
    drop(
        fixture
            .state
            .admit_managed_instance(&instance.id, false)
            .await
            .expect("retained latch remains recoverable later"),
    );
}

#[tokio::test]
async fn admitted_delete_claims_its_request_handoff_during_drain() {
    let fixture = TestFixture::new("delete-request-drain");
    let instance = add_test_instance(&fixture, "Delete during drain", "1.21.1");
    let request = fixture
        .state
        .try_admit_request()
        .expect("admit delete request before drain");
    let handoff = request.producer_handoff();
    let shutdown_state = fixture.state.clone();
    let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while fixture.state.lifecycle_phase() != crate::state::AppLifecyclePhase::DrainingRequests {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request drain begins");
    assert!(fixture.state.try_claim_producer().is_err());

    let body = handle_delete_instance_owned(&fixture.state, &instance.id, HashMap::new(), handoff)
        .await
        .expect("admitted delete completes during request drain");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));
    assert!(fixture.state.instances().get(&instance.id).is_none());

    drop(request);
    quiesce.abort();
    let _ = quiesce.await;
}

async fn wait_for_detached_delete(
    state: &AppState,
    instance_id: &str,
    retired_known_good: Option<&FsPath>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
            let registry_absent = state.instances().get(instance_id).is_none();
            let known_good_absent = retired_known_good.is_none_or(|path| !path.exists());
            drop(lifecycle);
            if registry_absent && known_good_absent {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached instance deletion did not finish");
}

async fn latch_managed_instance(fixture: &TestFixture, instance_id: &str) -> PathBuf {
    let staged = fixture
        .state
        .instances()
        .game_dir(instance_id)
        .join("mods/.axial-lock.json.new.tmp");
    fs::create_dir_all(staged.parent().expect("managed state parent"))
        .expect("create managed state directory");
    fs::write(&staged, b"not-json").expect("seed ambiguous managed publication");
    let admitted = fixture
        .state
        .admit_managed_instance(instance_id, false)
        .await
        .expect("admit managed inspection");
    admitted
        .inspect(None)
        .await
        .expect_err("ambiguous publication must latch managed identity");
    staged
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
        "failed for /home/zero/.config/Axial/instances/test/mods",
        "failed for C:\\Users\\Zero\\AppData\\Roaming\\Axial\\instances\\test\\logs",
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
    _request: RequestLease,
    producer: ProducerLease,
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
    let component_id = axial_minecraft::LoaderComponentId::Fabric;
    let build_id = axial_minecraft::build_id_for(component_id, minecraft_version, loader_version);
    write_fabric_loader_build_cache_with_builds(
        library_dir,
        minecraft_version,
        &[(loader_version, 100)],
        chrono::Utc::now().timestamp_millis(),
    );
    build_id
}

fn write_fabric_loader_build_cache_with_builds(
    library_dir: &FsPath,
    minecraft_version: &str,
    loader_versions: &[(&str, i32)],
    fetched_at_ms: i64,
) {
    let component_id = axial_minecraft::LoaderComponentId::Fabric;
    let index = axial_minecraft::LoaderVersionIndex {
        component_id,
        builds: loader_versions
            .iter()
            .map(|(loader_version, rank)| {
                fabric_build_record(component_id, minecraft_version, loader_version, *rank)
            })
            .collect(),
    };
    write_loader_build_cache_index(library_dir, minecraft_version, index, fetched_at_ms);
}

fn write_loader_build_cache_records(
    library_dir: &FsPath,
    component_id: axial_minecraft::LoaderComponentId,
    minecraft_version: &str,
    builds: Vec<axial_minecraft::LoaderBuildRecord>,
) {
    write_loader_build_cache_index(
        library_dir,
        minecraft_version,
        axial_minecraft::LoaderVersionIndex {
            component_id,
            builds,
        },
        chrono::Utc::now().timestamp_millis(),
    );
}

fn write_loader_build_cache_index(
    library_dir: &FsPath,
    minecraft_version: &str,
    index: axial_minecraft::LoaderVersionIndex,
    fetched_at_ms: i64,
) {
    let component_id = index.component_id;
    let cache = TestCachedCatalog {
        schema_version: axial_minecraft::LOADER_CATALOG_SCHEMA_VERSION,
        fetched_at_ms,
        value: index,
    };
    let cache_dir = axial_minecraft::loader_catalog_dir(library_dir);
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
}

fn fabric_build_record(
    component_id: axial_minecraft::LoaderComponentId,
    minecraft_version: &str,
    loader_version: &str,
    default_rank: i32,
) -> axial_minecraft::LoaderBuildRecord {
    let version_id =
        axial_minecraft::installed_version_id_for(component_id, minecraft_version, loader_version)
            .expect("valid loader identity");
    let build_id = axial_minecraft::build_id_for(component_id, minecraft_version, loader_version);
    axial_minecraft::LoaderBuildRecord {
        subject_kind: axial_minecraft::loaders::types::LoaderBuildSubjectKind::LoaderBuild,
        component_id,
        component_name: component_id.display_name().to_string(),
        build_id,
        minecraft_version: minecraft_version.to_string(),
        loader_version: loader_version.to_string(),
        version_id,
        build_meta: axial_minecraft::LoaderBuildMetadata {
            selection: axial_minecraft::LoaderSelectionMeta {
                default_rank,
                reason: axial_minecraft::LoaderSelectionReason::Recommended,
                source: axial_minecraft::LoaderSelectionSource::ExplicitApiFlag,
            },
            ..axial_minecraft::LoaderBuildMetadata::default()
        },
        strategy: axial_minecraft::LoaderInstallStrategy::FabricProfile,
        artifact_kind: axial_minecraft::LoaderArtifactKind::ProfileJson,
        installability: axial_minecraft::LoaderInstallability::Installable,
        install_source: axial_minecraft::loaders::LoaderInstallSource::ProfileJson {
            url: "https://example.invalid/fabric-profile.json".to_string(),
        },
    }
}

fn loader_catalog_state(fresh: bool, stale: bool) -> axial_minecraft::LoaderCatalogState {
    axial_minecraft::LoaderCatalogState {
        availability: axial_minecraft::LoaderAvailability {
            fresh,
            stale,
            cache_hit: stale,
            checked_at_ms: 1,
            last_success_at_ms: Some(1),
            last_error: None,
            last_failure_kind: None,
        },
    }
}

fn installed_loader_entry(build: &axial_minecraft::LoaderBuildRecord) -> VersionEntry {
    VersionEntry {
        subject_kind: axial_minecraft::VersionSubjectKind::InstalledVersion,
        id: build.version_id.clone(),
        raw_kind: "release".to_string(),
        release_time: String::new(),
        minecraft_meta: axial_minecraft::MinecraftVersionMeta::default(),
        lifecycle: axial_minecraft::LifecycleMeta::default(),
        inherits_from: build.minecraft_version.clone(),
        launchable: true,
        installed: true,
        status: "ready".to_string(),
        status_detail: String::new(),
        needs_install: String::new(),
        java_component: String::new(),
        java_major: 0,
        loader: Some(axial_minecraft::VersionLoaderAttachment {
            component_id: build.component_id,
            component_name: build.component_name.clone(),
            build_id: build.build_id.clone(),
            loader_version: build.loader_version.clone(),
            build_meta: build.build_meta.clone(),
        }),
    }
}

fn write_version_manifest_cache(library_dir: &FsPath, version_ids: &[&str]) {
    let cache_path = axial_minecraft::version_manifest_cache_path(library_dir);
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
    component_id: axial_minecraft::LoaderComponentId,
    version_ids: &[&str],
) {
    let versions = version_ids
        .iter()
        .map(|version_id| (*version_id, Some(true)))
        .collect::<Vec<_>>();
    write_supported_versions_cache_with_stable_hints(library_dir, component_id, &versions);
}

fn write_supported_versions_cache_with_stable_hints(
    library_dir: &FsPath,
    component_id: axial_minecraft::LoaderComponentId,
    version_ids: &[(&str, Option<bool>)],
) {
    let cache = TestCachedCatalog {
        schema_version: axial_minecraft::LOADER_CATALOG_SCHEMA_VERSION,
        fetched_at_ms: chrono::Utc::now().timestamp_millis(),
        value: version_ids
            .iter()
            .map(|(version_id, stable_hint)| {
                let analysis = axial_minecraft::analyze_minecraft_version(
                    version_id,
                    "release",
                    "",
                    *stable_hint,
                    &[],
                );
                axial_minecraft::LoaderGameVersion {
                    subject_kind: axial_minecraft::VersionSubjectKind::MinecraftVersion,
                    id: (*version_id).to_string(),
                    release_time: String::new(),
                    minecraft_meta: analysis.minecraft_meta,
                    lifecycle: analysis.lifecycle,
                    stable_hint: *stable_hint,
                }
            })
            .collect::<Vec<_>>(),
    };
    let cache_dir = axial_minecraft::loader_catalog_dir(library_dir);
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

fn write_version_with_missing_library(library_dir: &FsPath, version_id: &str) {
    let client = b"client";
    let library = b"library";
    write_version_json_value(
        library_dir,
        version_id,
        serde_json::json!({
            "id": version_id,
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "downloads": {
                "client": {
                    "sha1": sha1_hex(client),
                    "size": client.len()
                }
            },
            "javaVersion": { "component": "java-runtime-gamma", "majorVersion": 17 },
            "libraries": [{
                "name": "com.example:demo:1.0.0",
                "downloads": {
                    "artifact": {
                        "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                        "url": "https://example.invalid/demo-1.0.0.jar",
                        "sha1": sha1_hex(library),
                        "size": library.len()
                    }
                }
            }]
        }),
    );
    fs::write(
        library_dir
            .join("versions")
            .join(version_id)
            .join(format!("{version_id}.jar")),
        client,
    )
    .expect("write client jar");
}

fn write_version_with_missing_asset_object(library_dir: &FsPath, version_id: &str) {
    let client = b"client";
    let asset = b"asset";
    let asset_hash = sha1_hex(asset);
    let asset_index = serde_json::json!({
        "objects": {
            "sounds/step.ogg": { "hash": asset_hash, "size": asset.len() }
        }
    });
    let asset_index_bytes = serde_json::to_vec_pretty(&asset_index).expect("serialize asset index");
    let asset_index_path = library_dir
        .join("assets")
        .join("indexes")
        .join("test-assets.json");
    fs::create_dir_all(asset_index_path.parent().expect("asset index parent"))
        .expect("create asset index dir");
    fs::write(&asset_index_path, &asset_index_bytes).expect("write asset index");
    write_version_json_value(
        library_dir,
        version_id,
        serde_json::json!({
            "id": version_id,
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {
                "id": "test-assets",
                "sha1": sha1_hex(&asset_index_bytes),
                "size": asset_index_bytes.len()
            },
            "downloads": {
                "client": {
                    "sha1": sha1_hex(client),
                    "size": client.len()
                }
            },
            "javaVersion": { "component": "java-runtime-gamma", "majorVersion": 17 },
            "libraries": []
        }),
    );
    fs::write(
        library_dir
            .join("versions")
            .join(version_id)
            .join(format!("{version_id}.jar")),
        client,
    )
    .expect("write client jar");
}

fn write_version_with_corrupt_client_jar(library_dir: &FsPath, version_id: &str) {
    let expected_client = b"fresh-client";
    write_version_json_value(
        library_dir,
        version_id,
        serde_json::json!({
            "id": version_id,
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "downloads": {
                "client": {
                    "sha1": sha1_hex(expected_client),
                    "size": expected_client.len()
                }
            },
            "javaVersion": { "component": "java-runtime-gamma", "majorVersion": 17 },
            "libraries": []
        }),
    );
    fs::write(
        library_dir
            .join("versions")
            .join(version_id)
            .join(format!("{version_id}.jar")),
        b"wrong-client",
    )
    .expect("write corrupt client jar");
}

fn write_child_version_with_incomplete_parent(
    library_dir: &FsPath,
    child_version_id: &str,
    parent_version_id: &str,
) {
    write_version_json_value(
        library_dir,
        parent_version_id,
        serde_json::json!({
            "id": parent_version_id,
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": "java-runtime-gamma", "majorVersion": 17 },
            "libraries": []
        }),
    );
    let parent_dir = library_dir.join("versions").join(parent_version_id);
    fs::write(
        parent_dir.join(format!("{parent_version_id}.jar")),
        b"client",
    )
    .expect("write parent jar");
    fs::write(parent_dir.join(".incomplete"), "incomplete").expect("write parent marker");
    write_version_json_value(
        library_dir,
        child_version_id,
        serde_json::json!({
            "id": child_version_id,
            "type": "release",
            "inheritsFrom": parent_version_id,
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
            "libraries": []
        }),
    );
}

fn write_version_json_value(library_dir: &FsPath, version_id: &str, value: serde_json::Value) {
    let version_dir = library_dir.join("versions").join(version_id);
    fs::create_dir_all(&version_dir).expect("create version dir");
    fs::write(
        version_dir.join(format!("{version_id}.json")),
        serde_json::to_vec_pretty(&value).expect("serialize version json"),
    )
    .expect("write version json");
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn add_test_instance(
    fixture: &TestFixture,
    name: &str,
    version_id: &str,
) -> axial_config::Instance {
    fixture
        .state
        .instances()
        .insert_for_test(name.to_string(), version_id.to_string())
        .expect("add test instance")
}

async fn listed_instance(fixture: &TestFixture, instance_id: &str) -> EnrichedInstance {
    handle_list_instances(&fixture.state, &fixture.producer)
        .await
        .instances
        .into_iter()
        .find(|instance| instance.id == instance_id)
        .expect("listed instance")
}

fn assert_not_launch_action(instance: &EnrichedInstance) {
    assert!(!instance.launchable);
    assert_ne!(instance.launch_action.label, "Launch");
    assert_ne!(
        instance.launch_action.primary_action,
        axial_config::LaunchPrimaryAction::Launch
    );
}

fn write_installed_loader_version(
    library_dir: &FsPath,
    component_id: axial_minecraft::LoaderComponentId,
    minecraft_version: &str,
    loader_version: &str,
) {
    let version_id =
        axial_minecraft::installed_version_id_for(component_id, minecraft_version, loader_version)
            .expect("valid loader identity");
    let version_dir = library_dir.join("versions").join(&version_id);
    fs::create_dir_all(&version_dir).expect("create loader version dir");
    fs::write(
        version_dir.join(format!("{version_id}.json")),
        format!(
            r#"{{
                "id": "{version_id}",
                "type": "release",
                "inheritsFrom": "{minecraft_version}",
                "axialMaterialized": true,
                "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "libraries": []
            }}"#
        ),
    )
    .expect("write loader version json");
}

fn write_installed_checksumless_loader_version(
    library_dir: &FsPath,
    component_id: axial_minecraft::LoaderComponentId,
    minecraft_version: &str,
    loader_version: &str,
) {
    write_installed_loader_version(library_dir, component_id, minecraft_version, loader_version);
    let version_id =
        axial_minecraft::installed_version_id_for(component_id, minecraft_version, loader_version)
            .expect("valid loader identity");
    let version_path = library_dir
        .join("versions")
        .join(&version_id)
        .join(format!("{version_id}.json"));
    let mut version: serde_json::Value =
        serde_json::from_slice(&fs::read(&version_path).expect("read installed loader version"))
            .expect("parse installed loader version");
    version["libraries"] = serde_json::json!([{
        "name": "com.example:checksumless:1.0.0",
        "url": "https://example.invalid/"
    }]);
    fs::write(
        version_path,
        serde_json::to_vec_pretty(&version).expect("serialize checksumless loader version"),
    )
    .expect("write checksumless loader version");
    let library_path = library_dir
        .join("libraries")
        .join("com/example/checksumless/1.0.0/checksumless-1.0.0.jar");
    fs::create_dir_all(library_path.parent().expect("library parent"))
        .expect("create library directory");
    fs::write(library_path, b"present but unauthoritative").expect("write checksumless library");
}

impl TestFixture {
    fn new(name: &str) -> Self {
        let (state, root) = test_state(name);

        let request = state.try_admit_request().expect("admit fixture request");
        let producer = request
            .producer_handoff()
            .try_claim()
            .expect("claim fixture producer");

        Self {
            state,
            _request: request,
            producer,
            root,
        }
    }

    fn configure_create_manifest(&self, version_ids: &[&str]) -> PathBuf {
        let library_dir = self.root.join("library");
        self.state
            .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
        write_version_manifest_cache(&library_dir, version_ids);
        library_dir
    }
}

fn test_state(name: &str) -> (AppState, PathBuf) {
    let root = test_root(name);
    let paths = test_paths(&root);
    let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
    let instances = Arc::new(
        InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
            .expect("load instances"),
    );
    let state = AppState::new(AppStateInit {
        app_name: "Axial".to_string(),
        version: "test".to_string(),
        config,
        instances,
        installs: Arc::new(InstallStore::new()),
        sessions: Arc::new(SessionStore::new()),
        performance: Arc::new(
            PerformanceManager::load_for_startup(&paths.config_dir).expect("performance manager"),
        ),
        startup_warnings: Vec::new(),
        frontend_dir: root.join("frontend"),
    });
    (state, root)
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn test_root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "axial-api-instances-{name}-{}-{}",
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
        crash_evidence: None,
        healing: None,
        guardian: None,
        outcome: None,
        stages: Vec::new(),
    }
}
