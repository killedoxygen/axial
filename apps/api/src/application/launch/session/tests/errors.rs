use super::*;

#[test]
fn launch_layout_error_response_keeps_500_and_hides_unix_path_material() {
    let (status, Json(body)) = launch_layout_error_response(
        "/Users/alice/Library/Application Support/Axial/instances/survival: Permission denied (os error 13)",
    );

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        serde_json::json!({
            "error": "Could not prepare the instance folder. Check app data permissions and try again."
        })
    );
    let body = body.to_string();
    for fragment in [
        "/Users/alice",
        "Library/Application Support",
        "instances/survival",
        "Permission denied",
        "os error 13",
    ] {
        assert!(
            !body.contains(fragment),
            "launch layout error exposed raw fragment {fragment}"
        );
    }
}

#[test]
fn launch_layout_error_response_hides_windows_path_material_and_raw_io_text() {
    let (status, Json(body)) = launch_layout_error_response(
        r"C:\Users\Alice\AppData\Roaming\Axial\instances\creative: Access is denied. Read-only file system (os error 5)",
    );

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body["error"],
        "Could not prepare the instance folder. Check app data permissions and try again."
    );
    let body = body.to_string();
    for fragment in [
        r"C:\Users\Alice",
        "AppData",
        r"instances\creative",
        "Access is denied",
        "Read-only file system",
        "os error 5",
    ] {
        assert!(
            !body.contains(fragment),
            "launch layout error exposed raw fragment {fragment}"
        );
    }
}
